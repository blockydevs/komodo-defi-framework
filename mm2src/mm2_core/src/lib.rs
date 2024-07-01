use derive_more::Display;
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};

pub mod data_asker;
pub mod event_dispatcher;
pub mod mm_ctx;

#[derive(Clone, Copy, Display, PartialEq, Default)]
pub enum DbNamespaceId {
    #[display(fmt = "MAIN")]
    #[default]
    Main,
    #[display(fmt = "TEST_{}", _0)]
    Test(u64),
}

impl DbNamespaceId {
    pub fn for_test() -> DbNamespaceId {
        let mut rng = thread_rng();
        DbNamespaceId::Test(rng.gen())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Display, Serialize)]
#[serde(rename_all = "lowercase")]
/// The Electrum selection policy to use. To be provided in the MM2 configuration.
///
/// Multiple: All connections are activated simultaneously.
/// Selective: Only one connection is activated at a time (until it fails).
pub enum ConnectionManagerPolicy {
    Multiple,
    #[default]
    Selective,
}
