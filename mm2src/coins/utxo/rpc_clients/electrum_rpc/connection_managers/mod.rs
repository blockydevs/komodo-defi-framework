use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, Weak};

use super::client::ElectrumClientImpl;
use super::connection::{ElectrumConnection, ElectrumConnectionErr};
use common::executor::Timer;
use keys::Address;

use async_trait::async_trait;
use instant::Instant;

mod connection_context;
mod multiple;
mod selective;

pub use multiple::ConnectionManagerMultiple;
pub use selective::ConnectionManagerSelective;

/// Trait provides a common interface to get an `ElectrumConnection` from the `ElectrumClient` instance
#[async_trait]
pub trait ConnectionManagerTrait: Debug + Send + Sync {
    /// A copy of the connection manager.
    /// This is a workaround for the non-clonability of the objects/structs implementing this trait.
    fn copy(&self) -> Box<dyn ConnectionManagerTrait>;

    /// Initializes the connection manager by connecting the electrum connections.
    /// This must be called and only be called once to have a functioning connection manager.
    fn initialize(&self, weak_client: Weak<ElectrumClientImpl>) -> Result<(), ConnectionManagerErr>;

    /// Returns all the currently active connections.
    async fn get_active_connections(&self) -> Vec<Arc<ElectrumConnection>>;

    /// Waits until the connection manager is connected to the electrum server.
    async fn wait_till_connected(&self, timeout: f32) -> Result<(), String> {
        let start_time = Instant::now();
        loop {
            if !self.get_active_connections().await.is_empty() {
                return Ok(());
            }
            Timer::sleep(0.5).await;
            if start_time.elapsed().as_secs_f32() > timeout {
                return Err(format!(
                    "Waited for {} seconds but the connection manager is still not connected",
                    timeout
                ));
            }
        }
    }

    /// Returns all the server addresses.
    fn get_all_server_addresses(&self) -> Vec<String>;

    /// Retrieve a specific electrum connection by its address.
    /// The connection will be forcibly established if it's disconnected.
    async fn get_connection_by_address(
        &self,
        server_address: &str,
        force_connect: bool,
    ) -> Result<Arc<ElectrumConnection>, ConnectionManagerErr>;

    /// Returns a boolean value indicating whether the connections pool is empty (true)
    /// or not (false).
    async fn is_connections_pool_empty(&self) -> bool;

    /// Subscribes the address list to one/any of the active connection(s).
    async fn add_subscriptions(&self, addresses: &HashMap<String, Address>);

    // Handles the connection event.
    fn on_connected(&self, server_address: &str);

    // Handles the disconnection event from an Electrum server.
    fn on_disconnected(&self, server_address: &str);

    async fn remove_connection(&self, server_address: &str) -> Result<Arc<ElectrumConnection>, ConnectionManagerErr>;
}

#[derive(Debug, Display)]
pub enum ConnectionManagerErr {
    #[display(fmt = "Unknown server address")]
    UnknownAddress,
    #[display(fmt = "Failed to connect to the server due to {:?}", _0)]
    ConnectingError(ElectrumConnectionErr),
    #[display(fmt = "No client found, connection manager isn't initialized properly")]
    NoClient,
    #[display(fmt = "Connection manager is already initialized")]
    AlreadyInitialized,
}
