use async_trait::async_trait;
use core::time::Duration;
use futures::lock::{Mutex as AsyncMutex, MutexGuard};
use futures::{select, FutureExt};
use std::collections::HashMap;
use std::sync::Arc;

use crate::utxo::ScripthashNotificationSender;
use common::executor::abortable_queue::{AbortableQueue, WeakSpawner};
use common::executor::{AbortableSystem, SpawnFuture, Timer};
use common::log::{debug, error, info, warn};

use super::connection_manager_common::{ConnectionManagerErr, ConnectionManagerTrait, ElectrumConnCtx,
                                       DEFAULT_CONN_TIMEOUT_SEC, SUSPEND_TIMEOUT_INIT_SEC};
use super::{spawn_electrum, ElectrumClientEvent};
use super::{ElectrumConnSettings, ElectrumConnection};

#[derive(Clone, Debug)]
pub struct ConnectionManagerMultiple(pub Arc<ConnectionManagerMultipleImpl>);

impl ConnectionManagerMultiple {
    async fn suspend_server(&self, address: &str) -> Result<(), ConnectionManagerErr> {
        debug!(
            "About to suspend connection to addr: {}, guard: {:?}",
            address, self.0.state_guard
        );
        let mut guard = self.0.state_guard.lock().await;

        Self::reset_connection_context(&mut guard, address, self.0.abortable_system.create_subsystem().unwrap())?;

        let suspend_timeout_sec = Self::get_and_duplicate_suspend_timeout(&mut guard, address).await?;
        drop(guard);

        self.clone().spawn_resume_server(address, suspend_timeout_sec);
        debug!("Suspend future spawned");
        Ok(())
    }

    // workaround to avoid the cycle detected compilation error that blocks recursive async calls
    fn spawn_resume_server(self, address: &str, suspend_timeout_sec: u64) {
        let spawner = self.0.abortable_system.weak_spawner();
        let address = address.to_owned();
        spawner.spawn(Box::new(
            async move {
                debug!("Suspend server: {}, for: {} seconds", address, suspend_timeout_sec);
                Timer::sleep(suspend_timeout_sec as f64).await;
                let _ = self.resume_server(&address).await;
            }
            .boxed(),
        ));
    }

    async fn resume_server(self, address: &str) -> Result<(), ConnectionManagerErr> {
        debug!("Resume address: {}", address);
        let state_guard = self.0.state_guard.lock().await;

        let (_, conn_ctx) = state_guard.get_connection_ctx(address)?;
        let conn_settings = conn_ctx.conn_settings.clone();
        let conn_spawner = conn_ctx.abortable_system.weak_spawner();
        drop(state_guard);

        if let Err(err) = self.clone().connect_to(&conn_settings, conn_spawner).await {
            error!("Failed to resume: {}", err);
            self.suspend_server(address).await?;
        }
        Ok(())
    }

    fn reset_connection_context(
        state: &mut MutexGuard<'_, ConnectionManagerMultipleState>,
        address: &str,
        abortable_system: AbortableQueue,
    ) -> Result<(), ConnectionManagerErr> {
        debug!("Reset connection context for: {}", address);
        let (_, conn_ctx) = state.get_connection_ctx_mut(address)?;
        conn_ctx
            .abortable_system
            .abort_all()
            .map_err(|err| ConnectionManagerErr::FailedAbort(address.to_string(), err))?;
        conn_ctx.connection.take();
        conn_ctx.abortable_system = abortable_system;
        Ok(())
    }

    async fn get_and_duplicate_suspend_timeout(
        state: &mut MutexGuard<'_, ConnectionManagerMultipleState>,
        address: &str,
    ) -> Result<u64, ConnectionManagerErr> {
        let timeout = state
            .get_connection_ctx(address)
            .map(|(_, conn_ctx)| conn_ctx.suspend_timeout_sec)?;
        Self::set_suspend_timeout(state, address, |origin| origin.checked_mul(2).unwrap_or(u64::MAX))?;

        Ok(timeout)
    }

    fn reset_suspend_timeout(
        state: &mut MutexGuard<'_, ConnectionManagerMultipleState>,
        address: &str,
    ) -> Result<(), ConnectionManagerErr> {
        Self::set_suspend_timeout(state, address, |_| SUSPEND_TIMEOUT_INIT_SEC)
    }

    fn set_suspend_timeout<F: Fn(u64) -> u64>(
        state: &mut MutexGuard<'_, ConnectionManagerMultipleState>,
        address: &str,
        method: F,
    ) -> Result<(), ConnectionManagerErr> {
        let conn_ctx = state.get_connection_ctx_mut(address)?;
        let suspend_timeout = &mut conn_ctx.1.suspend_timeout_sec;
        let new_value = method(*suspend_timeout);
        debug!(
            "Set suspend timeout for address: {} - from: {} to the value: {}",
            address, suspend_timeout, new_value
        );
        *suspend_timeout = new_value;
        Ok(())
    }

    async fn connect_to(
        self,
        conn_settings: &ElectrumConnSettings,
        weak_spawner: WeakSpawner,
    ) -> Result<(), ConnectionManagerErr> {
        let (conn, mut conn_ready_receiver) = spawn_electrum(
            conn_settings,
            weak_spawner.clone(),
            self.0.event_sender.clone(),
            &self.0.scripthash_notification_sender,
        )?;
        Self::register_connection(&mut self.0.state_guard.lock().await, conn)?;
        let timeout_sec = conn_settings.timeout_sec.unwrap_or(DEFAULT_CONN_TIMEOUT_SEC);
        let address = conn_settings.url.clone();
        select! {
            _ = async_std::task::sleep(Duration::from_secs(timeout_sec)).fuse() => {
                self
                .suspend_server(&address)
                .await
            },
            _ = conn_ready_receiver => {
                ConnectionManagerMultiple::reset_suspend_timeout(&mut self.0.state_guard.lock().await, &address)
            }
        }
    }

    fn register_connection(
        state: &mut MutexGuard<'_, ConnectionManagerMultipleState>,
        conn: ElectrumConnection,
    ) -> Result<(), ConnectionManagerErr> {
        let (_, conn_ctx) = state.get_connection_ctx_mut(&conn.addr)?;
        conn_ctx.connection.replace(Arc::new(AsyncMutex::new(conn)));
        Ok(())
    }

    async fn get_connected(&self) -> Option<Arc<AsyncMutex<ElectrumConnection>>> {
        let subs = self.0.state_guard.lock().await;
        for connection in subs.connection_contexts.iter() {
            if let Some(connection) = &connection.connection {
                let conn = connection.lock().await;
                if conn.is_connected().await {
                    return Some(connection.clone());
                }
            }
        }

        None
    }
}

#[async_trait]
impl ConnectionManagerTrait for ConnectionManagerMultiple {
    async fn get_connection(&self) -> Vec<Arc<AsyncMutex<ElectrumConnection>>> { self.0.get_connection().await }

    async fn get_connection_by_address(
        &self,
        address: &str,
    ) -> Result<Arc<AsyncMutex<ElectrumConnection>>, ConnectionManagerErr> {
        self.0.get_connection_by_address(address).await
    }

    async fn connect(&self) -> Result<(), ConnectionManagerErr> {
        let mut state_guard = self.0.state_guard.lock().await;

        if state_guard.connection_contexts.is_empty() {
            return Err(ConnectionManagerErr::SettingsNotSet);
        }

        for context in &mut state_guard.connection_contexts {
            if context.connection.is_some() {
                let address = &context.conn_settings.url;
                warn!("An attempt to connect over an existing one: {}", address);
                continue;
            }
            let conn_settings = context.conn_settings.clone();
            let weak_spawner = context.abortable_system.weak_spawner();
            let self_clone = self.clone();
            self.0.abortable_system.weak_spawner().spawn(async move {
                let _ = self_clone.connect_to(&conn_settings, weak_spawner).await;
            });
        }

        Ok(())
    }

    async fn is_connected(&self) -> bool { self.0.is_connected().await }

    async fn remove_server(&self, address: &str) -> Result<(), ConnectionManagerErr> {
        self.0.remove_server(address).await
    }

    async fn rotate_servers(&self, no_of_rotations: usize) {
        debug!("Rotate servers: {}", no_of_rotations);
        let mut state_guard = self.0.state_guard.lock().await;
        state_guard.connection_contexts.rotate_left(no_of_rotations);
    }

    async fn is_connections_pool_empty(&self) -> bool { self.0.is_connections_pool_empty().await }

    async fn on_disconnected(&self, address: &str) {
        info!(
            "electrum_connection_manager disconnected from: {}, it will be suspended and trying to reconnect",
            address
        );
        let self_copy = self.clone();
        // check if any scripthash is subscribed to this addr and remove.
        self.remove_subscription_by_addr(address).await;

        let address = address.to_owned();
        self.0.abortable_system.weak_spawner().spawn(async move {
            if let Err(err) = self_copy.clone().suspend_server(&address).await {
                error!("Failed to suspend server: {}, error: {}", address, err);
            }
        });
    }

    async fn add_subscription(&self, script_hash: &str) {
        if let Some(connected) = self.get_connected().await {
            let mut subs = self.0.state_guard.lock().await;
            subs.scripthash_subs.insert(script_hash.to_string(), connected);
        };
    }

    async fn check_script_hash_subscription(&self, script_hash: &str) -> bool {
        let mut guard = self.0.state_guard.lock().await;
        // Find script_hash connection/subscription
        if let Some(connection) = guard.scripthash_subs.clone().get(script_hash) {
            let connection = connection.lock().await;
            // return true if there's an active connection.
            if connection.is_connected().await {
                return true;
            }
            //  Proceed to remove subscription if found but not connected/no connection..
            guard.scripthash_subs.remove(script_hash);
        };

        false
    }

    /// remove scripthash subscription from list by server addr
    async fn remove_subscription_by_addr(&self, server_addr: &str) {
        let mut guard = self.0.state_guard.lock().await;
        // remove server from scripthash subscription list
        for (script, conn) in guard.scripthash_subs.clone() {
            if conn.lock().await.addr == server_addr {
                guard.scripthash_subs.remove(&script);
            }
        }
    }
}

#[derive(Debug)]
pub struct ConnectionManagerMultipleImpl {
    state_guard: AsyncMutex<ConnectionManagerMultipleState>,
    abortable_system: AbortableQueue,
    event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
    scripthash_notification_sender: ScripthashNotificationSender,
}

impl ConnectionManagerMultipleImpl {
    pub(super) fn new(
        servers: Vec<ElectrumConnSettings>,
        abortable_system: AbortableQueue,
        event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
        scripthash_notification_sender: ScripthashNotificationSender,
    ) -> ConnectionManagerMultipleImpl {
        let mut connections: Vec<ElectrumConnCtx> = vec![];
        for conn_settings in servers {
            let subsystem: AbortableQueue = abortable_system.create_subsystem().unwrap();

            connections.push(ElectrumConnCtx {
                conn_settings,
                abortable_system: subsystem,
                suspend_timeout_sec: SUSPEND_TIMEOUT_INIT_SEC,
                connection: None,
            });
        }

        ConnectionManagerMultipleImpl {
            abortable_system,
            event_sender,
            state_guard: AsyncMutex::new(ConnectionManagerMultipleState {
                connection_contexts: connections,
                scripthash_subs: HashMap::new(),
            }),
            scripthash_notification_sender,
        }
    }

    async fn get_connection(&self) -> Vec<Arc<AsyncMutex<ElectrumConnection>>> {
        let connections = &self.state_guard.lock().await.connection_contexts;
        connections
            .iter()
            .filter(|conn_ctx| conn_ctx.connection.is_some())
            .map(|conn_ctx| conn_ctx.connection.as_ref().unwrap().clone())
            .collect()
    }

    async fn get_connection_by_address(
        &self,
        address: &str,
    ) -> Result<Arc<AsyncMutex<ElectrumConnection>>, ConnectionManagerErr> {
        let state_guard = self.state_guard.lock().await;
        let (_, conn_ctx) = state_guard.get_connection_ctx(address)?;
        conn_ctx
            .connection
            .as_ref()
            .cloned()
            .ok_or_else(|| ConnectionManagerErr::NotConnected(address.to_string()))
    }

    async fn is_connected(&self) -> bool {
        let state_guard = self.state_guard.lock().await;

        for conn_ctx in state_guard.connection_contexts.iter() {
            if let Some(ref connection) = conn_ctx.connection {
                return connection.lock().await.is_connected().await;
            }
        }

        false
    }

    async fn remove_server(&self, address: &str) -> Result<(), ConnectionManagerErr> {
        debug!("Remove electrum server: {}", address);
        let mut state_guard = self.state_guard.lock().await;
        let (i, _) = state_guard.get_connection_ctx(address)?;
        let conn_ctx = state_guard.connection_contexts.remove(i);
        conn_ctx
            .abortable_system
            .abort_all()
            .map_err(|err| ConnectionManagerErr::FailedAbort(address.to_string(), err))?;
        Ok(())
    }

    async fn is_connections_pool_empty(&self) -> bool { self.state_guard.lock().await.connection_contexts.is_empty() }
}

#[derive(Debug)]
struct ConnectionManagerMultipleState {
    connection_contexts: Vec<ElectrumConnCtx>,
    scripthash_subs: HashMap<String, Arc<AsyncMutex<ElectrumConnection>>>,
}

impl ConnectionManagerMultipleState {
    fn get_connection_ctx_mut(
        &mut self,
        address: &'_ str,
    ) -> Result<(usize, &mut ElectrumConnCtx), ConnectionManagerErr> {
        self.connection_contexts
            .iter_mut()
            .enumerate()
            .find(|(_, ctx)| ctx.conn_settings.url == address)
            .ok_or_else(|| ConnectionManagerErr::UnknownAddress(address.to_string()))
    }

    fn get_connection_ctx(&self, address: &str) -> Result<(usize, &ElectrumConnCtx), ConnectionManagerErr> {
        self.connection_contexts
            .iter()
            .enumerate()
            .find(|(_, c)| c.conn_settings.url == address)
            .ok_or_else(|| ConnectionManagerErr::UnknownAddress(address.to_string()))
    }
}
