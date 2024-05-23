/// Wait time before pinging again.
pub const PING_TIMEOUT_SEC: f64 = 30.;
/// The timeout for the electrum server to establish a connection (& verify server version).
pub const ELECTRUM_TIMEOUT_SEC: f64 = 60.;
pub const BLOCKCHAIN_HEADERS_SUB_ID: &str = "blockchain.headers.subscribe";
pub const BLOCKCHAIN_SCRIPTHASH_SUB_ID: &str = "blockchain.scripthash.subscribe";

/// FIXME(fix this doc): This timeout implies both connecting and verifying phases time.
pub const _DEFAULT_CONN_TIMEOUT_SEC: u64 = 20;
/// Initial server suspension time.
pub const SUSPEND_TIME_INIT_SEC: u64 = 30;
