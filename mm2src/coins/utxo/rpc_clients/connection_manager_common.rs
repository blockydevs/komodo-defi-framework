use async_trait::async_trait;
use common::executor::AbortedError;
use derive_more::Display;
use futures::lock::Mutex as AsyncMutex;
use std::fmt::Debug;
use std::ops::Deref;
use std::sync::Arc;

use super::ElectrumConnection;

/// Trait provides a common interface to get an `ElectrumConnection` from the `ElectrumClient` instance
#[async_trait]
pub trait ConnectionManagerTrait: Debug {
    async fn get_conn(&self) -> Vec<Arc<AsyncMutex<ElectrumConnection>>>;
    async fn get_conn_by_address(
        &self,
        address: &str,
    ) -> Result<Arc<AsyncMutex<ElectrumConnection>>, ConnectionManagerErr>;
    async fn connect(&self) -> Result<(), ConnectionManagerErr>;
    async fn is_connected(&self) -> bool;
    async fn remove_server(&self, address: &str) -> Result<(), ConnectionManagerErr>;
    async fn rotate_servers(&self, no_of_rotations: usize);
    async fn is_connections_pool_empty(&self) -> bool;
    async fn on_disconnected(&self, address: &str);

    /// Add a subscription for the given script hash to the connection manager's list of active subscriptions    
    async fn add_subscription(&self, script_hash: &str);

    /// Checks whether the specified script hash has an active subscription in the connection manager.
    /// It returns `true` if the script hash has an active subscription,
    /// otherwise returns `false`.
    async fn check_script_hash_subscription(&self, script_hash: &str) -> bool;

    /// Removes the subscription associated with the specified script hash from the connection manager.
    async fn remove_subscription_by_scripthash(&self, script_hash: &str);

    /// Removes all subscriptions associated with the specified server address from the connection manager.
    async fn remove_subscription_by_addr(&self, server_addr: &str);
}

#[async_trait]
impl ConnectionManagerTrait for Arc<dyn ConnectionManagerTrait + Send + Sync> {
    async fn get_conn(&self) -> Vec<Arc<AsyncMutex<ElectrumConnection>>> { self.deref().get_conn().await }
    async fn get_conn_by_address(
        &self,
        address: &str,
    ) -> Result<Arc<AsyncMutex<ElectrumConnection>>, ConnectionManagerErr> {
        self.deref().get_conn_by_address(address).await
    }
    async fn connect(&self) -> Result<(), ConnectionManagerErr> { self.deref().connect().await }
    async fn is_connected(&self) -> bool { self.deref().is_connected().await }
    async fn remove_server(&self, address: &str) -> Result<(), ConnectionManagerErr> {
        self.deref().remove_server(address).await
    }
    async fn rotate_servers(&self, no_of_rotations: usize) { self.deref().rotate_servers(no_of_rotations).await }
    async fn is_connections_pool_empty(&self) -> bool { self.deref().is_connections_pool_empty().await }
    async fn on_disconnected(&self, address: &str) { self.deref().on_disconnected(address).await }

    async fn add_subscription(&self, script_hash: &str) { self.deref().add_subscription(script_hash).await }

    async fn check_script_hash_subscription(&self, script_hash: &str) -> bool {
        self.deref().check_script_hash_subscription(script_hash).await
    }

    async fn remove_subscription_by_scripthash(&self, script_hash: &str) {
        self.deref().remove_subscription_by_scripthash(script_hash).await
    }

    async fn remove_subscription_by_addr(&self, server_addr: &str) {
        self.deref().remove_subscription_by_addr(server_addr).await
    }
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
