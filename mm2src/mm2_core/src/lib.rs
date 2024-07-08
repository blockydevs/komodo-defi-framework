use derive_more::Display;
use rand::{thread_rng, Rng};

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
