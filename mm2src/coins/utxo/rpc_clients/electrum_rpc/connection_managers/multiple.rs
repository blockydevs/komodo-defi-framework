use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock, Weak};

use super::super::client::{ElectrumClient, ElectrumClientImpl};
use super::super::connection::{ElectrumConnection, ElectrumConnectionErr, ElectrumConnectionSettings};
use super::super::constants::{PING_TIMEOUT_SEC, SUSPEND_TIME_INIT_SEC};
use super::{ConnectionManagerErr, ConnectionManagerTrait};

use crate::utxo::rpc_clients::UtxoRpcClientOps;
use common::executor::abortable_queue::AbortableQueue;
use common::executor::{AbortableSystem, SpawnFuture, Timer};
use common::log::warn;
use keys::Address;

use async_trait::async_trait;
use futures::compat::Future01CompatExt;
use gstuff::now_ms;

/// A struct that encapsulates an Electrum connection and its information.
#[derive(Debug)]
struct ConnectionContext {
    /// The electrum connection.
    connection: Arc<ElectrumConnection>,
    /// The list of addresses subscribed to the connection.
    subs: Mutex<Vec<Address>>,
    /// How long to suspend the server the next time it disconnects (in milliseconds).
    next_suspend_time: Mutex<u64>,
}

/// you wanna have a force wake method to wake up suspended servers that were queried specifically
#[derive(Debug)]
pub struct ConnectionManagerMultiple {
    /// A flag to spawn a ping loop task for each connection.
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
        abortable_system: AbortableQueue,
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
            connections.insert(connection.address().to_string(), ConnectionContext {
                connection: Arc::new(connection),
                subs: Mutex::new(Vec::new()),
                next_suspend_time: Mutex::new(SUSPEND_TIME_INIT_SEC),
            });
        }

        Ok(Arc::new(ConnectionManagerMultiple {
            spawn_ping,
            connections,
            electrum_client: RwLock::new(None),
        }))
    }

    /// Upgrades and returns the weak client we have. If None is returned this means either the client
    /// was never initialized (initialized will fix this), or the client was dropped, which in this case
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

        let background_task = {
            let manager = self.clone();
            let electrum_client = ElectrumClient(electrum_client.clone());
            async move {
                // First, Connect to all servers.
                for connection in manager.get_all_connections() {
                    ElectrumConnection::establish_connection_loop(connection, electrum_client.clone())
                        .await
                        .ok();
                }
                // Then, watch for disconnections to reconnect them back after timeout.
                watch_for_disconnections(manager).await;
            }
        };
        // Use the client's spawner to spawn the connection manager's background task.
        electrum_client.weak_spawner().spawn(background_task);

        if self.spawn_ping {
            let ping_task = {
                let manager = self.clone();
                async move {
                    loop {
                        let Some(client) = manager.get_client() else { break };
                        // This will ping all the active connections, which will keep these connections alive.
                        client.server_ping().compat().await.ok();
                        Timer::sleep(PING_TIMEOUT_SEC as f64).await;
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
    ) -> Result<Arc<ElectrumConnection>, ConnectionManagerErr> {
        let connection = self
            .get_connection(server_address)
            .ok_or_else(|| ConnectionManagerErr::UnknownAddress)?;

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

        Ok(connection)
    }

    async fn is_connections_pool_empty(&self) -> bool {
        // Since we don't remove the connections, but just set them as irrecoverable, we need
        // to check if all the connections are irrecoverable/not usable.
        let connections = self.get_all_connections();
        for connection in connections {
            match connection.last_error().await.map(|e| e.is_recoverable()) {
                None => return false,       // Connection is usable.
                Some(true) => return false, // Connection errored but recoverable.
                Some(false) => continue,    // Connection no longer usable.
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
                        connection_ctx.subs.lock().unwrap().push(address);
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

        // Reset the suspend time.
        *connection_ctx.next_suspend_time.lock().unwrap() = SUSPEND_TIME_INIT_SEC;
    }

    fn on_disconnected(&self, server_address: &str) {
        // The max time to suspend a server, 12h.
        const MAX_SUSPEND_TIME: u64 = 12 * 60 * 60;
        let Some(connection_ctx) = self.connections.get(server_address) else {
            warn!("No connection found for address: {server_address}");
            return;
        };
        // Clear the subs from the disconnected connection.
        let abandoned_subs = connection_ctx.subs.lock().unwrap().drain(..).collect();
        // Double the suspend time.
        let mut next_suspend_time = connection_ctx.next_suspend_time.lock().unwrap();
        *next_suspend_time = (*next_suspend_time * 2).min(MAX_SUSPEND_TIME);
        // Re-subscribe the abandoned addresses using the client.
        if let Some(client) = self.get_client() {
            client.subscribe_addresses(abandoned_subs).ok();
        };
    }

    /// Remove a connection from the connection manager by its address.
    // TODO(feat): Add the ability to add a connection during runtime.
    async fn remove_connection(&self, server_address: &str) -> Result<Arc<ElectrumConnection>, ConnectionManagerErr> {
        let Some(connection_ctx) = self.connections.get(server_address) else {
            return Err(ConnectionManagerErr::UnknownAddress);
        };
        // Make sure this connection is disconnected.
        connection_ctx
            .connection
            // Note that setting the error as irrecoverable renders the connection unusable.
            // This is how we (virtually) remove the connection right now. We never delete the
            // connection though. This is done for now to avoid guarding the connection map with a mutex.
            .disconnect(Some(ElectrumConnectionErr::Irrecoverable(
                "Forcefully disconnected & removed".to_string(),
            )))
            .await;
        // Run the on-disconnection hook.
        self.on_disconnected(connection_ctx.connection.address());
        Ok(connection_ctx.connection.clone())
    }
}

async fn watch_for_disconnections(manager: Arc<ConnectionManagerMultiple>) -> Option<()> {
    // A set of addresses and the time when they should be woken up, sorted by the time to be woken up.
    let mut wake_address_when = BTreeSet::new();
    // A set of addresses that we queued to wake.
    let mut addresses_to_wake = HashSet::new();

    loop {
        let now = now_ms();

        // If no connection is active, just fire up all the connections we have, we shouldn't operate
        // with no connections at all.
        if manager.get_active_connections().await.is_empty() {
            let client = manager.get_client()?;
            for connection in manager.get_all_connections() {
                ElectrumConnection::establish_connection_loop(connection, client.clone())
                    .await
                    .ok();
            }
            continue;
        }

        // Check for any new disconnections and keep track of them.
        for connection in manager.get_all_connections() {
            let address = connection.address().to_string();
            // Skip the connection if it's already in the wake list.
            if addresses_to_wake.contains(&address) {
                continue;
            }
            if !connection.is_connected().await {
                let Some(connection_ctx) = manager.connections.get(connection.address()) else {
                    // This shouldn't happen, continue anyway.
                    continue;
                };
                let suspend_until = now + *connection_ctx.next_suspend_time.lock().unwrap();
                // Register the address on our waking list.
                addresses_to_wake.insert(address.clone());
                wake_address_when.insert((suspend_until, address));
            }
        }

        // Reconnect the servers that are due to be waken up.
        let mut reconnected_addresses = HashSet::new();
        let client = manager.get_client()?;
        for (suspend_until, address) in wake_address_when.iter() {
            if now >= *suspend_until * 1000 {
                let connection = manager.get_connection(address)?;
                ElectrumConnection::establish_connection_loop(connection, client.clone())
                    .await
                    .ok();
                reconnected_addresses.insert((*suspend_until, address.clone()));
            } else {
                // We can break since the `suspend_until` is sorted ascendingly.
                break;
            }
        }
        for reconnected_address in reconnected_addresses {
            // Remove the address from the wake list.
            addresses_to_wake.remove(&reconnected_address.1);
            wake_address_when.remove(&reconnected_address);
        }
        drop(client);

        // Sleep for 5 seconds before checking again.
        Timer::sleep(5.).await;
    }
}
