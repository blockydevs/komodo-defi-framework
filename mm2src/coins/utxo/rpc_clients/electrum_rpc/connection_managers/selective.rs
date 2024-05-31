use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock, Weak};

use super::super::client::{ElectrumClient, ElectrumClientImpl};
use super::super::connection::{ElectrumConnection, ElectrumConnectionErr, ElectrumConnectionSettings};
use super::super::constants::PING_INTERVAL;
use super::connection_context::ConnectionContext;
use super::{ConnectionManagerErr, ConnectionManagerTrait};

use crate::utxo::rpc_clients::UtxoRpcClientOps;
use common::executor::abortable_queue::AbortableQueue;
use common::executor::{AbortableSystem, SpawnFuture, Timer};
use common::log::warn;
use futures::channel::mpsc;
use futures::StreamExt;
use keys::Address;

use async_trait::async_trait;
use futures::compat::Future01CompatExt;
use gstuff::now_ms;

#[derive(Debug)]
pub struct ConnectionManagerSelective {
    /// A flag to spawn a ping loop task for the active connection.
    spawn_ping: bool,
    /// The address currently active connection.
    active_address: Mutex<Option<String>>,
    /// A channel to notify the background task that some active connection has disconnected.
    active_address_disconnection_notifier: mpsc::Sender<()>,
    /// A receiver to be used by the background task to wait for disconnection notifications.
    /// We wrap it around a mutex to be able to take it out when the background task starts.
    active_address_disconnection_receiver: Mutex<Option<mpsc::Receiver<()>>>,
    /// A map for server addresses to their corresponding connections.
    connections: HashMap<String, ConnectionContext>,
    /// A weak reference to the electrum client that owns this connection manager.
    /// It is used to send electrum requests during connection establishment (version querying).
    // TODO: This field might not be necessary if [`ElectrumConnection`] object be used to send
    // electrum requests on its own, i.e. implement [`JsonRpcClient`] & [`UtxoRpcClientOps`].
    electrum_client: RwLock<Option<Weak<ElectrumClientImpl>>>,
}

impl ConnectionManagerSelective {
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

        // Create a channel to be shared between the connection manager and its background task
        // to notify the background task that some active connection has disconnected.
        let (sender, receiver) = mpsc::channel::<()>(0);

        Ok(Arc::new(ConnectionManagerSelective {
            spawn_ping,
            active_address: Mutex::new(None),
            connections,
            electrum_client: RwLock::new(None),
            active_address_disconnection_notifier: sender,
            active_address_disconnection_receiver: Mutex::new(Some(receiver)),
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

    fn get_primary_connections(&self) -> impl Iterator<Item = Arc<ElectrumConnection>> + '_ {
        self.get_all_connections().filter(|connection| connection.is_primary())
    }

    fn get_secondary_connections(&self) -> impl Iterator<Item = Arc<ElectrumConnection>> + '_ {
        self.get_all_connections()
            .filter(|connection| connection.is_secondary())
    }

    /// Returns a connection by its address.
    fn get_connection(&self, server_address: &str) -> Option<Arc<ElectrumConnection>> {
        self.connections
            .get(server_address)
            .map(|connection_ctx| connection_ctx.connection.clone())
    }

    /// Triggers the background task to reconnect to a server.
    fn trigger_reconnection(&self) {
        // Notify the background task that some active connection has disconnected.
        // Note that `try_send` might fail if the channel is out of capacity
        // (the manager already has an unpolled notification) or if the receiver
        // is dropped (which won't happen unless the background task is stopped).
        self.active_address_disconnection_notifier.clone().try_send(()).ok();
    }
}

// FIXME: A lot of the methods here are c/v from the multiple connection manager.
// Generalize by having them in the trait default implementation instead.
#[async_trait]
impl ConnectionManagerTrait for Arc<ConnectionManagerSelective> {
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
        // We might have some other connected servers (e.g. was connected because of connection by address),
        // but we will only return the one we know it's active right now. This also avoids sending pings to
        // connections that are not used thus keeping their connections alive.
        let maybe_active_connection = self
            .active_address
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|address| self.get_connection(address));
        if let Some(active_connection) = maybe_active_connection {
            println!("Active connection (not sure if connected): {}", active_connection.address());
            if active_connection.is_connected().await {
                println!("Active connection (connected): {}", active_connection.address());
                // There is only one active connection at a time.
                return vec![active_connection];
            }
        }
        println!("No active connection found.");
        // We don't currently have an active connection.
        self.trigger_reconnection();
        vec![]
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
        // FIXME: The multiple connection manager implementation of this method is still
        // compatible here. Let's just use both as trait default method.
        for (scripthash, address) in addresses.iter() {
            // Keep trying to subscribe the address until successful.
            loop {
                let Some(client) = self.get_client() else {
                    // The manager is either not initialized or the client is dropped.
                    // If the client is dropped, the manager is no longer usable.
                    return;
                };
                let active_connection = self.get_active_connections().await.first().cloned();
                let Some(active_connection) = active_connection else {
                    // If there is no active connection, wait for a connection to be established/activated.
                    Timer::sleep(1.).await;
                    continue;
                };
                if client
                    .blockchain_scripthash_subscribe_using(active_connection.address(), scripthash.clone())
                    .compat()
                    .await
                    .is_ok()
                {
                    if let Some(connection_ctx) = self.connections.get(active_connection.address()) {
                        connection_ctx.add_sub(address.clone());
                        // Address subscribed, move to the next one.
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
        connection_ctx.connected();
    }

    fn on_disconnected(&self, server_address: &str) {
        // If the disconnected connection is the active connection, mark that.
        let mut active_connection = self.active_address.lock().unwrap();
        if active_connection.as_deref() == Some(server_address) {
            *active_connection = None;
            self.trigger_reconnection();
        }
        drop(active_connection);

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

async fn background_task(manager: Arc<ConnectionManagerSelective>) {
    // Take out the notifier (receiver) from the connection manager.
    let Some(mut disconnection_notification) = manager.active_address_disconnection_receiver.lock().unwrap().take() else { return };
    loop {
        let Some(client) = manager.get_client() else { return };
        // We are disconnected at the point, try to connect to a server.
        // List all the primary connections first, then the secondary ones.
        let mut connections = manager.get_primary_connections().collect::<Vec<_>>();
        connections.extend(manager.get_secondary_connections());

        for connection in connections {
            let Some(connection_ctx) = manager.connections.get(connection.address()) else { continue };
            // Try to connect to the server if it's not suspended.
            println!("now ms: {}, suspend until: {}", now_ms(), connection_ctx.suspend_until());
            if now_ms() >= connection_ctx.suspend_until() {
                let address = connection.address().to_string();
                if ElectrumConnection::establish_connection_loop(connection, client.clone())
                    .await
                    .is_ok()
                {
                    // We are connected, mark the connection as active.
                    println!("assigned active connection: {address}");
                    *manager.active_address.lock().unwrap() = Some(address);
                    break;
                }
            }
        }

        //panic!("no connection found");
        println!("finished looking for a connection");
        if !manager.get_active_connections().await.is_empty() {
            // Since we are connected, wait for a disconnection notification
            // before we try connecting to another server.
            println!("wait till disconnection");
            disconnection_notification.next().await;
            println!("disconnected, looking for a new connection");
        }
    }
}
