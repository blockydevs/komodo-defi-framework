use derive_more::Display;
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};

pub mod data_asker;
pub mod event_dispatcher;
pub mod mm_ctx;

#[derive(Clone, Copy, Display, PartialEq)]
pub enum DbNamespaceId {
    #[display(fmt = "MAIN")]
    Main,
    #[display(fmt = "TEST_{}", _0)]
    Test(u64),
}

impl Default for DbNamespaceId {
    fn default() -> Self { DbNamespaceId::Main }
}

impl DbNamespaceId {
    pub fn for_test() -> DbNamespaceId {
        let mut rng = thread_rng();
        DbNamespaceId::Test(rng.gen())
    }
}

#[derive(Clone, Debug, Deserialize, Display, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionManagerPolicy {
    Multiple,
    Selective,
}

impl Default for ConnectionManagerPolicy {
    fn default() -> Self { ConnectionManagerPolicy::Selective }
}
