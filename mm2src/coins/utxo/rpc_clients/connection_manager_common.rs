use async_trait::async_trait;
use common::executor::AbortedError;
use derive_more::Display;
use futures::lock::Mutex as AsyncMutex;
use std::fmt::Debug;
use std::sync::Arc;

use super::ElectrumConnection;

/// Trait provides a common interface to get an `ElectrumConnection` from the `ElectrumClient` instance
#[async_trait]
pub trait ConnectionManagerTrait: Debug {
    /// Asynchronously retrieves all connections.
    async fn get_connection(&self) -> Vec<Arc<AsyncMutex<ElectrumConnection>>>;

    ///  Retrieve an electrum connection by its address.
    async fn get_connection_by_address(
        &self,
        address: &str,
    ) -> Result<Arc<AsyncMutex<ElectrumConnection>>, ConnectionManagerErr>;

    /// Asynchronously establishes connections to an/a electrum server(s).
    async fn connect(&self) -> Result<(), ConnectionManagerErr>;

    /// Check if an electrum server is connected
    async fn is_connected(&self) -> bool;

    /// Remove a server from connection manager by it's address.
    async fn remove_server(&self, address: &str) -> Result<(), ConnectionManagerErr>;

    /// Rotate the servers in the connection manager by a specified
    // number of rotations.
    async fn rotate_servers(&self, no_of_rotations: usize);

    /// Returns a boolean value indicating whether the connections pool is empty (true)
    // or not (false).
    async fn is_connections_pool_empty(&self) -> bool;

    // Handles the disconnection event from an Electrum server.
    async fn on_disconnected(&self, address: &str);

    /// Add a subscription for the given script hash to the connection manager's list of active subscriptions .   
    async fn add_subscription(&self, script_hash: &str);

    /// Checks whether the specified script hash has an active subscription in the connection manager.
    /// It returns `true` if the script hash has an active subscription,
    /// otherwise returns `false`.
    async fn check_script_hash_subscription(&self, script_hash: &str) -> bool;

    /// Removes all subscriptions associated with the specified server address from the connection manager.
    async fn remove_subscription_by_addr(&self, server_addr: &str);
}

#[derive(Debug, Display)]
pub enum ConnectionManagerErr {
    #[display(fmt = "Unknown address: {}", _0)]
    UnknownAddress(String),
    #[display(fmt = "Connection is not established, {}", _0)]
    NotConnected(String),
    #[display(fmt = "Failed to abort abortable system for: {}, error: {}", _0, _1)]
    FailedAbort(String, AbortedError),
    #[display(fmt = "Failed to connect to: {}, error: {}", _0, _1)]
    ConnectingError(String, String),
    #[display(fmt = "No settings to connect to found")]
    SettingsNotSet,
}

/// This timeout implies both connecting and verifying phases time
pub const DEFAULT_CONN_TIMEOUT_SEC: u64 = 20;
pub const SUSPEND_TIMEOUT_INIT_SEC: u64 = 30;
