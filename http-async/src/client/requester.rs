use std::{
    any::TypeId,
    collections::hash_map::Entry,
    fmt::Display,
    hash::Hash,
    ops::Deref,
    str::FromStr,
    sync::{Arc, Weak},
};

use async_trait::async_trait;
use chrono::{Duration, NaiveDateTime, Utc};
use either::Either;
use fxhash::FxHashMap;
use gw2api_model::{BulkEndpoint, Endpoint, EndpointWithId, FixedEndpoint, Language};
use hyper::{http::uri::PathAndQuery, Request, Response, Uri};
use serde::de::DeserializeOwned;
use tokio::sync::{
    broadcast::{self, Receiver, Sender},
    Mutex,
};

use crate::{
    cache::hash, Cache, CachedRequest, Client, EndpointError, EndpointResult, Inflight, RateLimiter,
};

#[async_trait]
pub trait Requester<const AUTHENTICATED: bool, const FORCE: bool>: Sized + Sync {
    type Caching: Cache + Send;
    type RateLimiting: RateLimiter + Sync;

    #[doc(hidden)]
    fn client(&self) -> &Client<Self::Caching, Self::RateLimiting, AUTHENTICATED>;

    #[doc(hidden)]
    fn cache_duration(&self) -> Duration;

    /// overwrites the cache duration for all requests returned from this
    /// function ## Example
    /// ```
    /// use chrono::Duration;
    /// use gw2api_http::{Client, Requester};
    /// use gw2api_http::gw2api_model::items::Item;
    /// use gw2api_http::gw2api_model::misc::build::Build;
    ///
    /// let client = Client::default();
    /// let cache_client = client.cached(Duration::seconds(5));
    /// // these requests get cached for 5s
    /// let build_id: Build = cache_client.get().unwrap();
    /// let item: Item = cache_client.single(19993).unwrap();
    /// // normal caching
    /// let other_item: Item = client.single(19721).unwrap();
    fn cached(
        &self,
        cache_duration: Duration,
    ) -> CachedRequest<Self::Caching, Self::RateLimiting, AUTHENTICATED, FORCE> {
        CachedRequest {
            client: self.client(),
            cache_duration,
        }
    }

    /// forces a fresh copy from the api
    /// ## Example
    /// ```
    /// use gw2api_http::{Client, Requester};
    /// use gw2api_http::gw2api_model::misc::build::Build;
    ///
    /// let client = Client::default();
    /// let build_id: Build = client.get().unwrap();
    /// // this is cached and won't hit the api
    /// let build_id: Build = client.get().unwrap();
    /// // this will not check the cache and ask the api directly
    /// let build_id: Build = client.forced().get().unwrap();
    fn forced(&self) -> CachedRequest<Self::Caching, Self::RateLimiting, AUTHENTICATED, true> {
        CachedRequest {
            client: self.client(),
            cache_duration: Duration::zero(),
        }
    }

    /// call the fixed endpoint
    async fn get<T: DeserializeOwned + Clone + Send + Sync + FixedEndpoint + 'static>(
        &self,
    ) -> EndpointResult<T> {
        get_or_ids::<T, T, Self, AUTHENTICATED, FORCE>(self).await
    }

    /// request a single item
    async fn single<
        T: DeserializeOwned + Clone + Send + Sync + EndpointWithId<IdType = I> + 'static,
        I: Display + DeserializeOwned + Hash + Send + Sync + Clone + 'static,
    >(
        &self,
        id: I,
    ) -> EndpointResult<T> {
        let lang = self.client().language;
        if let Some(c) = self.try_get(&id).await {
            return Ok(c);
        }

        let tx = loop {
            let either = check_inflight::<T, I, T>(&self.client().inflight, &id, lang).await;
            match either {
                Some(Either::Left(mut rx)) => return rx.recv().await.map_err(Into::into),
                Some(Either::Right(tx)) => break tx,
                None => {
                    if let Some(c) = self.try_get(&id).await {
                        return Ok(c);
                    }
                }
            }
        };

        let url = T::format_url(&self.client().host, &id);
        let request = build_request::<T, String, Self, AUTHENTICATED, FORCE>(self, &url, None)?;

        let response = self.client().client.request(request).await?;
        let result =
            cache_response::<I, T, T, Self, AUTHENTICATED, FORCE>(self, &id, response).await?;
        // ignoring the error is fine here
        // the receiving side will check the cache if nothing got sent
        let _ = tx.lock().await.send(result.clone());

        Ok(result)
    }

    /// retrieves an item from cache
    /// ```
    /// use gw2api_http::{gw2api_model::items::Item, Client, Requester};
    ///
    /// let client = Client::default();
    /// let from_cache: Option<Item> = client.try_get(&19721);
    /// ```
    async fn try_get<
        T: DeserializeOwned + Clone + Endpoint + Send + Sync + 'static,
        I: DeserializeOwned + Hash + Clone + Sync + 'static,
    >(
        &self,
        id: &I,
    ) -> Option<T> {
        check_cache::<T, I, T, Self, AUTHENTICATED, FORCE>(self, id).await
    }

    /// request all available ids
    async fn ids<
        T: DeserializeOwned + EndpointWithId<IdType = I> + Clone + Send + Sync + 'static,
        I: Display + DeserializeOwned + Hash + Clone + Send + Sync + 'static,
    >(
        &self,
    ) -> EndpointResult<Vec<I>> {
        get_or_ids::<T, Vec<I>, Self, AUTHENTICATED, FORCE>(self).await
    }

    /// request multiple ids at once
    async fn many<
        T: DeserializeOwned
            + EndpointWithId<IdType = I>
            + BulkEndpoint
            + Clone
            + Send
            + Sync
            + 'static,
        I: Display + DeserializeOwned + Hash + Clone + Eq + Send + Sync + 'static,
    >(
        &self,
        mut ids: Vec<I>,
    ) -> EndpointResult<Vec<T>> {
        let mut result = Vec::with_capacity(ids.len());
        if !FORCE {
            ids = extract_many_from_cache(self, ids, &mut result).await;
            if ids.is_empty() {
                return Ok(result);
            }
        }

        let mut txs = FxHashMap::with_capacity_and_hasher(ids.len(), Default::default());
        let mut rxs = Vec::with_capacity(ids.len());
        let mut remaining_ids = Vec::with_capacity(ids.len());
        for id in ids {
            let retain = loop {
                let either =
                    check_inflight::<T, I, T>(&self.client().inflight, &id, self.client().language)
                        .await;
                match either {
                    Some(Either::Left(rx)) => {
                        rxs.push(rx);
                        break false;
                    }
                    Some(Either::Right(tx)) => {
                        txs.insert(id.clone(), tx);
                        break true;
                    }
                    None => {
                        if let Some(c) =
                            check_cache::<T, I, T, Self, AUTHENTICATED, FORCE>(self, &id).await
                        {
                            result.push(c);
                            break false;
                        }
                    }
                }
            };
            if retain {
                remaining_ids.push(id);
            }
        }

        let result = Mutex::new(result);
        let txs = Mutex::new(txs);
        let url = format!("{}/{}", self.client().host, T::URL);
        let chunks = join_ids(&remaining_ids);
        let futs = chunks.into_iter().map(|rest| {
            let rest = Some(format!("ids={rest}"));
            async {
                let request = build_request::<T, _, Self, AUTHENTICATED, FORCE>(self, &url, rest)?;

                let response = self.client().client.request(request).await?;
                let mut result = result.lock().await;
                let index = result.len();
                cache_response_many(self, response, &mut result).await?;

                let mut txs = txs.lock().await;
                for x in result.iter().skip(index) {
                    let tx = txs
                        .remove(x.id())
                        .expect("received unexpected entry from api");
                    // ignoring the error is fine here
                    // the receiving side will check the cache if nothing got sent
                    let _ = tx.lock().await.send(x.clone());
                }
                Result::<(), EndpointError>::Ok(())
            }
        });
        futures::future::try_join_all(futs).await?;

        let mut result = result.into_inner();
        for mut rx in rxs {
            result.push(rx.recv().await?);
        }

        Ok(result)
    }

    /// requests a page of items and returns the number of total items across
    /// all pages
    async fn page<
        T: DeserializeOwned
            + EndpointWithId<IdType = I>
            + BulkEndpoint
            + Clone
            + Send
            + Sync
            + 'static,
        I: Display + DeserializeOwned + Hash + Clone + Sync + 'static,
    >(
        &self,
        page: usize,
        page_size: u8,
        result: &mut Vec<T>,
    ) -> EndpointResult<usize> {
        let url = format!("{}/{}", self.client().host, T::URL);
        let queries = format!("page={}&page_size={}", page, page_size);
        let request = build_request::<T, _, Self, AUTHENTICATED, FORCE>(self, &url, Some(queries))?;

        let response = self.client().client.request(request).await?;
        let count = get_header(&response, "x-result-total").unwrap_or(0);
        cache_response_many(self, response, result).await?;

        Ok(count)
    }

    /// requests all items using the most efficient method available
    /// ### Remarks
    /// for most endpoints this means using [`Self::get_all_by_requesting_ids`].
    /// Compared to [`Self::get_all_by_paging`]
    /// this needs to perform an additional request to get all ids, but is much
    /// more cache friendly, being able to utilize the cache and inflight
    /// mechanisms.
    async fn all<
        T: DeserializeOwned
            + EndpointWithId<IdType = I>
            + BulkEndpoint
            + Clone
            + Send
            + Sync
            + 'static,
        I: Display + DeserializeOwned + Hash + Clone + Send + Sync + Eq + 'static,
    >(
        &self,
    ) -> EndpointResult<Vec<T>> {
        if T::ALL {
            self.get_all_by_ids_all().await
        // paging cannot utilize the cache, so we won't use it by default
        // } else if T::PAGING {
        //     self.get_all_by_paging()
        } else {
            self.get_all_by_requesting_ids().await
        }
    }

    /// Gets all items by querying ids=all
    ///
    /// use [`Self::all`] to use the most efficient way to request all items
    async fn get_all_by_ids_all<
        T: DeserializeOwned
            + EndpointWithId<IdType = I>
            + BulkEndpoint
            + Clone
            + Send
            + Sync
            + 'static,
        I: Display + DeserializeOwned + Hash + Clone + Sync + 'static,
    >(
        &self,
    ) -> EndpointResult<Vec<T>> {
        if !T::ALL {
            return Err(EndpointError::UnsupportedEndpointQuery);
        }

        let url = format!("{}/{}", self.client().host, T::URL);
        let request =
            build_request::<T, _, Self, AUTHENTICATED, FORCE>(self, &url, Some("ids=all"))?;

        let response = self.client().client.request(request).await?;
        let count = get_header(&response, "x-result-total").unwrap_or(0);
        let mut result = Vec::with_capacity(count);
        cache_response_many(self, response, &mut result).await?;

        Ok(result)
    }

    /// Gets all items by querying all pages
    ///
    /// use [`Self::all`] to use the most efficient way to request all items
    async fn get_all_by_paging<
        T: DeserializeOwned
            + EndpointWithId<IdType = I>
            + BulkEndpoint
            + Clone
            + Send
            + Sync
            + 'static,
        I: Display + DeserializeOwned + Hash + Clone + Sync + 'static,
    >(
        &self,
    ) -> EndpointResult<Vec<T>> {
        if !T::PAGING {
            return Err(EndpointError::UnsupportedEndpointQuery);
        }

        let mut result = Vec::with_capacity(200);
        let max_items = self.page(0, 200, &mut result).await?;
        let remaining = max_items.saturating_sub(200);
        result.reserve_exact(remaining);

        let pages = ((remaining as f64) / 200_f64).ceil() as usize;
        for page in 0..pages {
            self.page(page + 1, 200, &mut result).await?;
        }

        Ok(result)
    }

    /// Gets all items by querying all ids
    ///
    /// use [`Self::all`] to use the most efficient way to request all items
    async fn get_all_by_requesting_ids<
        T: DeserializeOwned
            + EndpointWithId<IdType = I>
            + BulkEndpoint
            + Clone
            + Send
            + Sync
            + 'static,
        I: Display + DeserializeOwned + Hash + Clone + Send + Sync + Eq + 'static,
    >(
        &self,
    ) -> EndpointResult<Vec<T>> {
        let ids = self.ids::<T, I>().await?;
        self.many(ids).await
    }
}

struct SenderGuard<'client, T: Send> {
    sender: Arc<Mutex<Sender<T>>>,
    inflight: &'client Inflight,
    hash: (TypeId, u64),
}

impl<T: Send> Deref for SenderGuard<'_, T> {
    type Target = Mutex<Sender<T>>;

    fn deref(&self) -> &Self::Target {
        &self.sender
    }
}

impl<T: Send> Drop for SenderGuard<'_, T> {
    fn drop(&mut self) {
        let inflight = self.inflight.clone();
        let hash = self.hash;
        tokio::spawn(async move { inflight.lock().await.remove(&hash) });
    }
}

async fn check_inflight<
    'client,
    H: Send + Clone + 'static,
    I: 'static + Hash,
    T: Endpoint + Send + 'static,
>(
    inflight: &'client Inflight,
    id: &I,
    lang: Language,
) -> Option<Either<Receiver<H>, SenderGuard<'client, H>>> {
    let hash = hash::<H, I>(id, T::LOCALE.then(|| lang));
    let mut locked = inflight.lock().await;
    Some(match locked.entry(hash) {
        Entry::Occupied(mut e) => {
            let r = e
                .get_mut()
                .downcast_mut::<Weak<Mutex<Sender<H>>>>()
                .unwrap();
            let r = r.upgrade()?;
            let r = r.lock().await;
            Either::Left(r.subscribe())
        }
        Entry::Vacant(e) => {
            let tx = Arc::new(Mutex::new(broadcast::channel(1).0));
            e.insert(Box::new(Arc::downgrade(&tx)));
            let tx = SenderGuard {
                sender: tx,
                inflight,
                hash,
            };
            Either::Right(tx)
        }
    })
}

async fn check_cache<
    T: Clone + Send + Sync + 'static,
    I: Hash + Sync + 'static,
    E: Endpoint,
    Req: Requester<A, F>,
    const A: bool,
    const F: bool,
>(
    req: &Req,
    id: &I,
) -> Option<T> {
    if !F {
        let mut cache = req.client().cache.lock().await;
        cache.get::<T, I, E>(id, req.client().language).await
    } else {
        None
    }
}

async fn get_or_ids<
    T: DeserializeOwned + Endpoint + Clone + Send + Sync + 'static,
    K: DeserializeOwned + Clone + Send + Sync + 'static,
    Req: Requester<A, F>,
    const A: bool,
    const F: bool,
>(
    req: &Req,
) -> EndpointResult<K> {
    let lang = req.client().language;
    if let Some(c) = check_cache::<K, (), T, Req, A, F>(req, &()).await {
        return Ok(c);
    }

    let tx = loop {
        let either = check_inflight::<K, (), T>(&req.client().inflight, &(), lang).await;
        match either {
            Some(Either::Left(mut rx)) => return rx.recv().await.map_err(Into::into),
            Some(Either::Right(tx)) => break tx,
            None => {
                if let Some(c) = check_cache::<K, (), T, Req, A, F>(req, &()).await {
                    return Ok(c);
                }
            }
        }
    };

    let url = format!("{}/{}", req.client().host, T::URL);
    let request = build_request::<T, String, Req, A, F>(req, &url, None)?;

    let response = req.client().client.request(request).await?;
    let result = cache_response::<(), K, T, Req, A, F>(req, &(), response).await?;
    // ignoring the error is fine here
    // the receiving side will check the cache if nothing got sent
    let _ = tx.lock().await.send(result.clone());

    Ok(result)
}

fn build_request<
    T: Endpoint,
    Q: Into<String>,
    Req: Requester<A, F>,
    const A: bool,
    const F: bool,
>(
    req: &Req,
    url: &str,
    extra_queries: Option<Q>,
) -> Result<Request<hyper::Body>, EndpointError> {
    let mut request = hyper::Request::builder().uri(url);

    if T::AUTHENTICATED && !A {
        return Err(EndpointError::NotAuthenticated);
    }

    request = request.header("X-Schema-Version", T::VERSION);
    if T::AUTHENTICATED {
        request = request.header(
            "Authorization",
            &format!("Bearer {}", req.client().api_key.as_ref().unwrap()),
        );
    }

    let mut request = request.body(hyper::Body::empty()).unwrap();
    let mut args = Vec::new();
    if T::LOCALE {
        args.push(format!("lang={}", req.client().language.as_str()));
    }
    if let Some(ids) = extra_queries {
        args.push(ids.into());
    }
    if !args.is_empty() {
        let mut parts = request.uri().clone().into_parts();
        if let Some(pathnquery) = &mut parts.path_and_query {
            let args = args.join("&");
            let query = if let Some(query) = pathnquery.query() {
                format!("{}&{}", query, args)
            } else {
                format!("?{}", args)
            };
            *pathnquery = PathAndQuery::from_str(&format!("{}{}", pathnquery.path(), query))
                .expect("invalid path and query");
            let uri = Uri::from_parts(parts).expect("invalid uri");
            *request.uri_mut() = uri;
        };
    }

    Ok(request)
}

/// returns the remaining ids not found in cache
async fn extract_many_from_cache<
    I: Display + Hash + Sync + 'static,
    K: EndpointWithId<IdType = I> + Clone + Send + Sync + 'static,
    Req: Requester<A, F>,
    const A: bool,
    const F: bool,
>(
    req: &Req,
    ids: Vec<I>,
    result: &mut Vec<K>,
) -> Vec<I> {
    let mut rest = Vec::with_capacity(ids.len());
    let mut cache = req.client().cache.lock().await;
    for i in ids {
        if let Some(cached) = cache.get::<K, I, K>(&i, req.client().language).await {
            result.push(cached);
        } else {
            rest.push(i);
        }
    }
    rest
}

async fn cache_response<
    I: Hash + Sync + 'static,
    K: DeserializeOwned + Clone + Send + Sync + 'static,
    T: Endpoint,
    Req: Requester<A, F>,
    const A: bool,
    const F: bool,
>(
    req: &Req,
    id: &I,
    response: Response<hyper::Body>,
) -> Result<K, EndpointError> {
    let expires = get_cache_expiry(req, &response);
    let body = hyper::body::to_bytes(response.into_body()).await?;
    let result: K = serde_json::from_slice(&body)?;
    let res = result.clone();
    {
        let mut cache = req.client().cache.lock().await;
        cache
            .insert::<K, I, T>(id, res, expires, req.client().language)
            .await;
    }
    Ok(result)
}

async fn cache_response_many<
    I: Display + Hash + Sync + 'static,
    K: DeserializeOwned + BulkEndpoint + EndpointWithId<IdType = I> + Clone + Send + Sync + 'static,
    Req: Requester<A, F>,
    const A: bool,
    const F: bool,
>(
    req: &Req,
    response: Response<hyper::Body>,
    result: &mut Vec<K>,
) -> Result<(), EndpointError> {
    let expires = get_cache_expiry(req, &response);
    let body = hyper::body::to_bytes(response.into_body()).await?;
    let res: Vec<K> = serde_json::from_slice(&body)?;
    {
        let mut cache = req.client().cache.lock().await;
        for t in res {
            cache
                .insert::<K, I, K>(t.id(), t.clone(), expires, req.client().language)
                .await;
            result.push(t);
        }
    }
    Ok(())
}

fn get_cache_expiry<Req: Requester<A, F>, const A: bool, const F: bool>(
    req: &Req,
    response: &Response<hyper::Body>,
) -> NaiveDateTime {
    let duration = req.cache_duration();
    let expires = if !duration.is_zero() {
        duration
    } else {
        get_expire_from_header(response)
    };
    Utc::now().naive_utc() + expires
}

/// concatenates ids, separated by comma: 1,2,3,4
/// chunked in 200 per batch
///
/// panics when `ids.len() == 0`
fn join_ids<I: Display + 'static>(ids: &[I]) -> Vec<String> {
    use std::fmt::Write;
    let modulo = ids.len() % 200 != 0;
    let ceil = ids.len() / 200 + (modulo as usize);
    let mut result = Vec::with_capacity(ceil);
    for ids in ids.chunks(200) {
        let mut query_string = String::with_capacity(6 * ids.len()); // arbitrary. most ids are 5 digits + comma
        write!(&mut query_string, "{}", ids[0]).expect("failed to concatenate ids");
        for i in ids.iter().skip(1) {
            write!(&mut query_string, ",{}", i).expect("failed to concatenate ids");
        }
        result.push(query_string);
    }
    result
}

fn get_expire_from_header(response: &Response<hyper::Body>) -> Duration {
    let exp = get_header(response, "cache-control").unwrap_or(300);
    Duration::seconds(exp)
}

fn get_header<T: FromStr>(response: &Response<hyper::Body>, header: &str) -> Option<T> {
    response
        .headers()
        .iter()
        .find(|x| x.0 == header)
        .and_then(|(_, d)| d.to_str().ok())
        .and_then(|d| d.parse::<T>().ok())
}