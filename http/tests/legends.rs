#![cfg(feature = "blocking")]

use gw2lib::{
	model::game_mechanics::legends::{Legend, LegendId},
	Requester
};

pub mod setup;

#[test]
fn all() {
    let client = setup::setup();
    assert!(client.all::<Legend, LegendId>().is_ok());
}

// sigh, anet. Maybe someday it'll show up in the API
#[test]
#[ignore]
fn alliance_legend_exists() {
    let client = setup::setup();
	assert!(client.single::<Legend, LegendId>(String::from("Legend7")).is_ok());
}
