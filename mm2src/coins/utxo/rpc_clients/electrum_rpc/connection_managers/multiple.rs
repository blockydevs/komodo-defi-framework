use std::collections::HashMap;
use std::sync::{Arc, RwLock, Weak};

use super::super::client::{ElectrumClient, ElectrumClientImpl};
use super::super::connection::{ElectrumConnection, ElectrumConnectionErr, ElectrumConnectionSettings};
use super::super::constants::PING_INTERVAL;
use super::connection_context::ConnectionContext;
use super::{ConnectionManagerErr, ConnectionManagerTrait};

use crate::utxo::rpc_clients::UtxoRpcClientOps;
use common::executor::abortable_queue::AbortableQueue;
use common::executor::{AbortableSystem, SpawnFuture, Timer};
use common::log::warn;
use keys::Address;

use async_trait::async_trait;
use futures::compat::Future01CompatExt;
use gstuff::now_ms;

#[derive(Debug)]
pub struct ConnectionManagerMultiple {
    /// A flag to spawn a ping loop task for active connections.
    spawn_ping: bool,
    /// A map for server addresses to their corresponding connections.
    connections: HashMap<String, ConnectionContext>,
    /// A weak reference to the electrum client that owns this connection manager.
    /// It is used to send electrum requests during connection establishment (version querying).
    // TODO: This field might not be necessary if [`ElectrumConnection`] object be used to send
    // electrum requests on its own, i.e. implement [`JsonRpcClient`] & [`UtxoRpcClientOps`].
    electrum_client: RwLock<Option<Weak<ElectrumClientImpl>>>,
}

impl ConnectionManagerMultiple {
    pub fn try_new_arc(
        servers: Vec<ElectrumConnectionSettings>,
        spawn_ping: bool,
        abortable_system: &AbortableQueue,
    ) -> Result<Arc<Self>, String> {
        let mut connections = HashMap::with_capacity(servers.len());
        for connection_settings in servers {
            let subsystem = abortable_system.create_subsystem().map_err(|e| {
                ERRL!(
                    "Failed to create abortable subsystem for connection: {}, error: {:?}",
                    connection_settings.url,
                    e
                )
            })?;

            let connection = ElectrumConnection::new(connection_settings, subsystem);
            connections.insert(connection.address().to_string(), ConnectionContext::new(connection));
        }

        Ok(Arc::new(ConnectionManagerMultiple {
            spawn_ping,
            connections,
            electrum_client: RwLock::new(None),
        }))
    }

    /// Attempts to upgrades and return the weak client reference we hold. If None is returned this means either
    /// the client was never initialized (initialized will fix this), or the client was dropped, which in this case
    /// the connection manager is no longer usable.
    fn get_client(&self) -> Option<ElectrumClient> {
        self.electrum_client
            .read()
            .unwrap()
            .as_ref()
            .and_then(|weak| weak.upgrade().map(ElectrumClient))
    }

    /// Returns an iterator over all the connections we have, even removed ones.
    fn get_all_connections(&self) -> impl Iterator<Item = Arc<ElectrumConnection>> + '_ {
        self.connections
            .values()
            .map(|connection_ctx| connection_ctx.connection.clone())
    }

    /// Returns a connection by its address.
    fn get_connection(&self, server_address: &str) -> Option<Arc<ElectrumConnection>> {
        self.connections
            .get(server_address)
            .map(|connection_ctx| connection_ctx.connection.clone())
    }
}

#[async_trait]
impl ConnectionManagerTrait for Arc<ConnectionManagerMultiple> {
    fn copy(&self) -> Box<dyn ConnectionManagerTrait> { Box::new(self.clone()) }

    fn initialize(&self, weak_client: Weak<ElectrumClientImpl>) -> Result<(), ConnectionManagerErr> {
        // Disallow reusing the same manager with another client.
        if self.electrum_client.read().unwrap().is_some() {
            return Err(ConnectionManagerErr::AlreadyInitialized);
        }

        let Some(electrum_client) = weak_client.upgrade() else {
            return Err(ConnectionManagerErr::NoClient);
        };

        // Store the (weak) electrum client.
        *self.electrum_client.write().unwrap() = Some(weak_client);

        // Use the client's spawner to spawn the connection manager's background task.
        electrum_client.weak_spawner().spawn(background_task(self.clone()));

        if self.spawn_ping {
            let ping_task = {
                let manager = self.clone();
                async move {
                    loop {
                        let Some(client) = manager.get_client() else { break };
                        // This will ping all the active connections, which will keep these connections alive.
                        client.server_ping().compat().await.ok();
                        Timer::sleep(PING_INTERVAL).await;
                    }
                }
            };
            // Use the client's spawner to spawn the connection manager's ping task.
            electrum_client.weak_spawner().spawn(ping_task);
        }

        Ok(())
    }

    async fn get_active_connections(&self) -> Vec<Arc<ElectrumConnection>> {
        let mut connections = vec![];
        for connection in self.get_all_connections() {
            if connection.is_connected().await {
                connections.push(connection);
            }
        }
        connections
    }

    fn get_all_server_addresses(&self) -> Vec<String> { self.connections.keys().cloned().collect() }

    async fn get_connection_by_address(
        &self,
        server_address: &str,
        force_connect: bool,
    ) -> Result<Arc<ElectrumConnection>, ConnectionManagerErr> {
        let connection = self
            .get_connection(server_address)
            .ok_or_else(|| ConnectionManagerErr::UnknownAddress)?;

        if force_connect {
            // Force connect the connection if it's not connected yet.
            if !connection.is_connected().await {
                if let Some(client) = self.get_client() {
                    ElectrumConnection::establish_connection_loop(connection.clone(), client)
                        .await
                        .map_err(ConnectionManagerErr::ConnectingError)?;
                } else {
                    return Err(ConnectionManagerErr::NoClient);
                }
            }
        }

        Ok(connection)
    }

    async fn is_connections_pool_empty(&self) -> bool {
        // Since we don't remove the connections, but just set them as irrecoverable, we need
        // to check if all the connections are irrecoverable/not usable.
        for connection in self.get_all_connections() {
            if connection.usable().await {
                return false;
            }
        }
        true
    }

    /// Subscribe the list of addresses to our active connections.
    ///
    /// There is a bit of indirection here. We register the abandoned addresses on `on_disconnected` with
    /// the client to queue them for `utxo_balance_events` which in turn calls this method back to re-subscribe
    /// the abandoned addresses. We could have instead directly re-subscribed the addresses here in the connection
    /// manager without sending them to `utxo_balance_events`. However, we don't do that so that `utxo_balance_events`
    /// knows about all the added addresses. If it doesn't know about them, it won't be able to retrieve the triggered
    /// address when its script hash is notified.
    // FIXME: We don't check if we are already subscribed to that address. If we unintentiontally subscribe one address
    // to multiple servers, we will get multiple notifications for the same balance change. RefreshSubscription message breaks
    // this guarantee. Either remove it or guard against that by checking if we are already subscribed to that address.
    async fn add_subscriptions(&self, addresses: &HashMap<String, Address>) {
        let mut addresses = addresses.clone();
        loop {
            let Some((scripthash, address)) = addresses.iter().next().map(|(s, a)| (s.clone(), a.clone())) else {
                // `addresses` set is empty, subscribed all the addresses.
                break;
            };
            let Some(client) = self.get_client() else {
                // The manager is either not initialized or the client is dropped.
                // If the client is dropped, the manager is no longer usable.
                return;
            };
            let connections = self.get_active_connections().await;
            if connections.is_empty() {
                // If there are no active connections, wait for a connection to be established.
                Timer::sleep(1.).await;
                continue;
            }
            // Try to subscribe the address to any connection we have.
            for connection in connections {
                if client
                    .blockchain_scripthash_subscribe_using(connection.address(), scripthash.clone())
                    .compat()
                    .await
                    .is_ok()
                {
                    if let Some(connection_ctx) = self.connections.get(connection.address()) {
                        connection_ctx.add_sub(address);
                        // This address has been registered to a connection, remove it from the list.
                        addresses.remove(&scripthash);
                        break;
                    }
                }
            }
        }
    }

    fn on_connected(&self, server_address: &str) {
        let Some(connection_ctx) = self.connections.get(server_address) else {
            warn!("No connection found for address: {server_address}");
            return;
        };

        // Reset the suspend time & disconnection time.
        connection_ctx.connected();
    }

    fn on_disconnected(&self, server_address: &str) {
        let Some(connection_ctx) = self.connections.get(server_address) else {
            warn!("No connection found for address: {server_address}");
            return;
        };
        let abandoned_subs = connection_ctx.disconnected();
        // Re-subscribe the abandoned addresses using the client.
        if let Some(client) = self.get_client() {
            client.subscribe_addresses(abandoned_subs.into_iter().collect()).ok();
        };
    }

    /// Remove a connection from the connection manager by its address.
    // TODO(feat): Add the ability to add a connection during runtime.
    async fn remove_connection(&self, server_address: &str) -> Result<Arc<ElectrumConnection>, ConnectionManagerErr> {
        let connection = self
            .get_connection(server_address)
            .ok_or_else(|| ConnectionManagerErr::UnknownAddress)?;
        // Make sure this connection is disconnected.
        connection
            // Note that setting the error as irrecoverable renders the connection unusable.
            // This is how we (virtually) remove the connection right now. We never delete the
            // connection though. This is done for now to avoid guarding the connection map with a mutex.
            .disconnect(Some(ElectrumConnectionErr::Irrecoverable(
                "Forcefully disconnected & removed".to_string(),
            )))
            .await;
        // Run the on-disconnection hook.
        self.on_disconnected(connection.address());
        Ok(connection.clone())
    }
}

async fn background_task(manager: Arc<ConnectionManagerMultiple>) {
    loop {
        let Some(client) = manager.get_client() else { return };
        // If no connection is active, just fire up all the connections we have, we shouldn't operate
        // with no connections at all.
        if manager.get_active_connections().await.is_empty() {
            for connection in manager.get_all_connections() {
                ElectrumConnection::establish_connection_loop(connection, client.clone())
                    .await
                    .ok();
            }
            continue;
        }
        // Check for any disconnected connection that are due to being connected.
        for connection in manager.get_all_connections() {
            if !connection.is_connected().await {
                let Some(connection_ctx) = manager.connections.get(connection.address()) else { continue };
                // Try to connect the server if it's time to wake it up.
                if now_ms() >= connection_ctx.suspend_until() {
                    ElectrumConnection::establish_connection_loop(connection, client.clone())
                        .await
                        .ok();
                }
            }
        }
        drop(client);

        // Sleep for 5 seconds before checking again.
        Timer::sleep(5.).await;
    }
}
