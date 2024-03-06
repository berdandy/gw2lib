use serde::{Deserialize, Serialize};

use crate::*;
pub use crate::game_mechanics::skills::SkillId;
pub use crate::authenticated::characters::LegendId;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub struct PetSkill {
	pub id: SkillId,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, serde(deny_unknown_fields))]
pub struct Legend {
	pub id: String,
	pub code: u8,
	pub swap: SkillId,
	pub heal: SkillId,
	pub elite: SkillId,
	pub utilities: [SkillId; 3],
}

impl Endpoint for Legend {
    const AUTHENTICATED: bool = false;
    const LOCALE: bool = true;
    const URL: &'static str = "v2/legends";
    const VERSION: &'static str = "2022-07-22T00:00:00.000Z";
}

impl EndpointWithId for Legend {
	type IdType = LegendId;
}

impl BulkEndpoint for Legend {
    const ALL: bool = true;

    fn id(&self) -> &Self::IdType {
        &self.id
    }
}
