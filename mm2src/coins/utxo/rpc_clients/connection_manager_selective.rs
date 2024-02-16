use crate::utxo::ScripthashNotificationSender;

use async_trait::async_trait;
use common::executor::abortable_queue::{AbortableQueue, WeakSpawner};
use common::executor::{AbortableSystem, SpawnFuture, Timer};
use common::log::{debug, error, info, warn};
use futures::future::FutureExt;
use futures::lock::{Mutex as AsyncMutex, MutexGuard};
use futures::select;
use mm2_rpc::data::legacy::Priority;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;

use super::connection_manager_common::{ConnectionManagerErr, ConnectionManagerTrait, ElectrumConnCtx,
                                       DEFAULT_CONN_TIMEOUT_SEC, SUSPEND_TIMEOUT_INIT_SEC};
use super::{spawn_electrum, ElectrumClientEvent, ElectrumConnSettings, ElectrumConnection};

#[derive(Clone, Debug)]
pub struct ConnectionManagerSelective(pub Arc<ConnectionManagerSelectiveImpl>);

impl ConnectionManagerSelective {
    async fn fetch_conn_settings(&self) -> Option<(ElectrumConnSettings, WeakSpawner, ConnectingAtomicCtx)> {
        let mut guard = self.0.state_guard.lock().await;
        if guard.active.is_some() {
            warn!("Skip connecting, already connected");
            return None;
        }

        let mng_spawner = self.0.abortable_system.weak_spawner();
        let Some(connecting_state_ctx) = ConnectingAtomicCtx::try_new(& mut guard, self.clone(), mng_spawner) else {
            warn!("Skip connecting, is in progress");
            return None
        };

        debug!("Primary electrum nodes to connect: {:?}", guard.primary_connections);
        debug!("Backup electrum nodes to connect: {:?}", guard.backup_connections);
        let mut iter = guard.primary_connections.iter().chain(guard.backup_connections.iter());
        let addr = iter.next()?.clone();
        if let Ok(conn_ctx) = Self::get_conn_ctx(&guard, &addr) {
            Some((
                conn_ctx.conn_settings.clone(),
                conn_ctx.abortable_system.weak_spawner(),
                connecting_state_ctx,
            ))
        } else {
            warn!("Failed to connect, no connection settings found");
            None
        }
    }

    async fn suspend_server(&self, address: String) -> Result<(), ConnectionManagerErr> {
        debug!(
            "About to suspend connection to addr: {}, guard: {:?}",
            address, self.0.state_guard
        );
        let mut guard = self.0.state_guard.lock().await;
        if let Some(ref active) = guard.active {
            if *active == address {
                guard.active.take();
            }
        }

        match Self::get_conn_ctx(&guard, &address)?.conn_settings.priority {
            Priority::Primary => {
                guard.primary_connections.pop_front();
            },
            Priority::Secondary => {
                guard.backup_connections.pop_front();
            },
        };

        Self::reset_connection_context(
            &mut guard,
            &address,
            self.0.abortable_system.create_subsystem().unwrap(),
        )?;

        let suspend_timeout_sec = Self::get_suspend_timeout(&guard, &address).await?;
        Self::duplicate_suspend_timeout(&mut guard, &address)?;
        drop(guard);

        self.clone().spawn_resume_server(address, suspend_timeout_sec);
        debug!("Suspend future spawned");
        Ok(())
    }

    // workaround to avoid the cycle detected compilation error that blocks recursive async calls
    fn spawn_resume_server(self, address: String, suspend_timeout_sec: u64) {
        let spawner = self.0.abortable_system.weak_spawner();
        spawner.spawn(Box::new(
            async move {
                debug!("Suspend server: {}, for: {} seconds", address, suspend_timeout_sec);
                Timer::sleep(suspend_timeout_sec as f64).await;
                let _ = self.resume_server(address).await;
            }
            .boxed(),
        ));
    }

    async fn resume_server(self, address: String) -> Result<(), ConnectionManagerErr> {
        debug!("Resume address: {}", address);
        let mut guard = self.0.state_guard.lock().await;
        let priority = Self::get_conn_ctx(&guard, &address)?.conn_settings.priority.clone();
        match priority {
            Priority::Primary => guard.primary_connections.push_back(address.clone()),
            Priority::Secondary => guard.backup_connections.push_back(address.clone()),
        }

        if let Some(active) = guard.active.clone() {
            let conn_ctx = Self::get_conn_ctx(&guard, &address)?;
            let active_ctx = Self::get_conn_ctx(&guard, &active)?;
            let active_priority = &active_ctx.conn_settings.priority;
            if let (Priority::Secondary, Priority::Primary) = (active_priority, priority) {
                let conn_settings = conn_ctx.conn_settings.clone();
                let conn_spawner = conn_ctx.abortable_system.weak_spawner();
                drop(guard);
                if let Err(err) = self
                    .clone()
                    .connect_to(
                        conn_settings,
                        conn_spawner,
                        self.0.event_sender.clone(),
                        &self.0.scripthash_notification_sender,
                    )
                    .await
                {
                    error!("Failed to resume: {}", err);
                    self.suspend_server(address.clone()).await?;
                } else {
                    let mut guard = self.0.state_guard.lock().await;
                    Self::reset_connection_context(
                        &mut guard,
                        &active,
                        self.0.abortable_system.create_subsystem().unwrap(),
                    )?;
                    ConnectionManagerSelectiveImpl::set_active_connection(&mut guard, address.clone())?;
                }
            }
        } else {
            drop(guard);
            let _ = self.connect().await;
        };
        Ok(())
    }

    fn reset_connection_context(
        state: &mut MutexGuard<'_, ConnectionManagerSelectiveState>,
        address: &str,
        abortable_system: AbortableQueue,
    ) -> Result<(), ConnectionManagerErr> {
        debug!("Reset connection context for: {}", address);

        let conn_ctx = Self::get_conn_ctx_mut(state, address)?;
        conn_ctx
            .abortable_system
            .abort_all()
            .map_err(|err| ConnectionManagerErr::FailedAbort(address.to_string(), err))?;
        conn_ctx.connection.take();
        conn_ctx.abortable_system = abortable_system;
        Ok(())
    }

    fn register_connection(
        state: &mut MutexGuard<'_, ConnectionManagerSelectiveState>,
        conn: ElectrumConnection,
    ) -> Result<(), ConnectionManagerErr> {
        let conn_ctx = Self::get_conn_ctx_mut(state, &conn.addr)?;
        conn_ctx.connection.replace(Arc::new(AsyncMutex::new(conn)));
        Ok(())
    }

    async fn get_suspend_timeout(
        state: &MutexGuard<'_, ConnectionManagerSelectiveState>,
        address: &str,
    ) -> Result<u64, ConnectionManagerErr> {
        Self::get_conn_ctx(state, address).map(|ctx| ctx.suspend_timeout_sec)
    }

    fn duplicate_suspend_timeout(
        state: &mut MutexGuard<'_, ConnectionManagerSelectiveState>,
        address: &str,
    ) -> Result<(), ConnectionManagerErr> {
        Self::set_suspend_timeout(state, address, |origin| origin.checked_mul(2).unwrap_or(u64::MAX))
    }

    fn reset_suspend_timeout(
        state: &mut MutexGuard<'_, ConnectionManagerSelectiveState>,
        address: &str,
    ) -> Result<(), ConnectionManagerErr> {
        Self::set_suspend_timeout(state, address, |_| SUSPEND_TIMEOUT_INIT_SEC)
    }

    fn set_suspend_timeout<F: Fn(u64) -> u64>(
        state: &mut MutexGuard<'_, ConnectionManagerSelectiveState>,
        address: &str,
        method: F,
    ) -> Result<(), ConnectionManagerErr> {
        let conn_ctx = Self::get_conn_ctx_mut(state, address)?;
        let suspend_timeout = &mut conn_ctx.suspend_timeout_sec;
        let new_value = method(*suspend_timeout);
        debug!(
            "Set supsend timeout for address: {} - from: {} to the value: {}",
            address, suspend_timeout, new_value
        );
        *suspend_timeout = new_value;
        Ok(())
    }

    async fn connect_to(
        &self,
        conn_settings: ElectrumConnSettings,
        weak_spawner: WeakSpawner,
        event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
        scripthash_notification_sender: &ScripthashNotificationSender,
    ) -> Result<(), ConnectionManagerErr> {
        let (conn, mut conn_ready_receiver) = spawn_electrum(
            &conn_settings,
            weak_spawner.clone(),
            event_sender,
            scripthash_notification_sender,
        )?;
        Self::register_connection(&mut self.0.state_guard.lock().await, conn)?;
        let timeout_sec = conn_settings.timeout_sec.unwrap_or(DEFAULT_CONN_TIMEOUT_SEC);

        select! {
            _ = async_std::task::sleep(Duration::from_secs(timeout_sec)).fuse() => {
                warn!("Failed to connect to: {}, timed out", conn_settings.url);
                Err(ConnectionManagerErr::ConnectingError(conn_settings.url.clone(), format!("Timed out: {}", timeout_sec)))
            },
            _ = conn_ready_receiver => Ok(()) // TODO: handle cancelled
        }
    }

    fn get_conn_ctx<'a>(
        state: &'a MutexGuard<'a, ConnectionManagerSelectiveState>,
        address: &str,
    ) -> Result<&'a ElectrumConnCtx, ConnectionManagerErr> {
        state
            .connection_contexts
            .get(address)
            .ok_or_else(|| ConnectionManagerErr::UnknownAddress(address.to_string()))
    }

    fn get_conn_ctx_mut<'a, 'b>(
        state: &'a mut MutexGuard<'b, ConnectionManagerSelectiveState>,
        address: &'_ str,
    ) -> Result<&'a mut ElectrumConnCtx, ConnectionManagerErr> {
        state
            .connection_contexts
            .get_mut(address)
            .ok_or_else(|| ConnectionManagerErr::UnknownAddress(address.to_string()))
    }
}

#[async_trait]
impl ConnectionManagerTrait for ConnectionManagerSelective {
    async fn get_connection(&self) -> Vec<Arc<AsyncMutex<ElectrumConnection>>> { self.0.get_connection().await }

    async fn get_connection_by_address(
        &self,
        address: &str,
    ) -> Result<Arc<AsyncMutex<ElectrumConnection>>, ConnectionManagerErr> {
        self.0.get_connection_by_address(address).await
    }

    async fn connect(&self) -> Result<(), ConnectionManagerErr> {
        while let Some((conn_settings, weak_spawner, _connecting_state_ctx)) = self.fetch_conn_settings().await {
            debug!("Got conn_settings to connect to: {:?}", conn_settings);
            let address = conn_settings.url.clone();
            match self
                .connect_to(
                    conn_settings,
                    weak_spawner,
                    self.0.event_sender.clone(),
                    &self.0.scripthash_notification_sender,
                )
                .await
            {
                Ok(_) => {
                    ConnectionManagerSelectiveImpl::set_active_connection(
                        &mut self.0.state_guard.lock().await,
                        address,
                    )?;
                    break;
                },
                Err(_) => {
                    self.clone().suspend_server(address.clone()).await?;
                },
            };
        }
        Ok(())
    }

    async fn is_connected(&self) -> bool { self.0.is_connected().await }

    async fn remove_server(&self, address: &str) -> Result<(), ConnectionManagerErr> {
        self.0.remove_server(address).await
    }

    async fn rotate_servers(&self, _no_of_rotations: usize) {
        // not implemented for the conn mng selective
    }

    async fn is_connections_pool_empty(&self) -> bool { self.0.is_connections_pool_empty().await }

    async fn on_disconnected(&self, address: &str) {
        info!(
            "electrum_connection_manager disconnected from: {}, it will be suspended and trying to reconnect",
            address
        );
        let self_copy = self.clone();
        let address = address.to_string();
        // check if any scripthash is subscribed to this addr and remove.
        self.remove_subscription_by_addr(&address).await;
        self.0.abortable_system.weak_spawner().spawn(async move {
            if let Err(err) = self_copy.clone().suspend_server(address.clone()).await {
                error!("Failed to suspend server: {}, error: {}", address, err);
            }
            if let Err(err) = self_copy.connect().await {
                error!(
                    "Failed to reconnect after addr was disconnected: {}, error: {}",
                    address, err
                );
            }
        });
    }

    async fn add_subscription(&self, script_hash: &str) {
        let mut guard = self.0.state_guard.lock().await;
        if let Some(active) = &guard.active {
            if let Some(conn) = Self::get_conn_ctx(&guard, active).unwrap().connection.clone() {
                guard.scripthash_subs.insert(script_hash.to_string(), conn);
            };
        };
    }

    async fn check_script_hash_subscription(&self, script_hash: &str) -> bool {
        let mut guard = self.0.state_guard.lock().await;
        // Find script_hash connection/subscription
        if let Some(connection) = guard.scripthash_subs.clone().get(script_hash) {
            let connection = connection.lock().await;
            if connection.is_connected().await {
                return true;
            }

            //  Proceed to remove subscription if found but not connected/no connection..
            guard.scripthash_subs.remove(script_hash);
        };

        false
    }

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
pub struct ConnectionManagerSelectiveImpl {
    state_guard: AsyncMutex<ConnectionManagerSelectiveState>,
    abortable_system: AbortableQueue,
    event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
    scripthash_notification_sender: ScripthashNotificationSender,
}
impl ConnectionManagerSelectiveImpl {
    pub(super) fn try_new(
        servers: Vec<ElectrumConnSettings>,
        abortable_system: AbortableQueue,
        event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
        scripthash_notification_sender: ScripthashNotificationSender,
    ) -> Result<ConnectionManagerSelectiveImpl, String> {
        let mut primary_connections = VecDeque::<String>::new();
        let mut backup_connections = VecDeque::<String>::new();
        let mut connection_contexts: BTreeMap<String, ElectrumConnCtx> = BTreeMap::new();
        for conn_settings in servers {
            match conn_settings.priority {
                Priority::Primary => primary_connections.push_back(conn_settings.url.clone()),
                Priority::Secondary => backup_connections.push_back(conn_settings.url.clone()),
            }
            let conn_abortable_system = abortable_system.create_subsystem().map_err(|err| {
                ERRL!(
                    "Failed to create abortable subsystem for conn: {}, error: {}",
                    conn_settings.url,
                    err
                )
            })?;
            let _ = connection_contexts.insert(conn_settings.url.clone(), ElectrumConnCtx {
                conn_settings,
                connection: None,
                abortable_system: conn_abortable_system,
                suspend_timeout_sec: SUSPEND_TIMEOUT_INIT_SEC,
            });
        }

        Ok(ConnectionManagerSelectiveImpl {
            event_sender,
            state_guard: AsyncMutex::new(ConnectionManagerSelectiveState {
                connecting: AtomicBool::new(false),
                primary_connections,
                backup_connections,
                active: None,
                connection_contexts,
                scripthash_subs: HashMap::new(),
            }),
            scripthash_notification_sender,
            abortable_system,
        })
    }

    async fn remove_server(&self, address: &str) -> Result<(), ConnectionManagerErr> {
        debug!("Remove server: {}", address);
        let mut guard = self.state_guard.lock().await;
        let conn_ctx = guard
            .connection_contexts
            .remove(address)
            .ok_or_else(|| ConnectionManagerErr::UnknownAddress(address.to_string()))?;

        match conn_ctx.conn_settings.priority {
            Priority::Primary => guard.primary_connections.pop_front(),
            Priority::Secondary => guard.backup_connections.pop_front(),
        };
        if let Some(active) = guard.active.as_ref() {
            if active == address {
                guard.active.take();
            }
        }
        Ok(())
    }

    fn set_active_connection(
        guard: &mut MutexGuard<'_, ConnectionManagerSelectiveState>,
        address: String,
    ) -> Result<(), ConnectionManagerErr> {
        ConnectionManagerSelective::reset_suspend_timeout(guard, &address)?;
        let _ = guard.active.replace(address);
        Ok(())
    }

    async fn is_connected(&self) -> bool { self.state_guard.lock().await.active.is_some() }

    async fn is_connections_pool_empty(&self) -> bool { self.state_guard.lock().await.connection_contexts.is_empty() }

    async fn get_connection(&self) -> Vec<Arc<AsyncMutex<ElectrumConnection>>> {
        debug!("Getting available connection");
        let guard = self.state_guard.lock().await;
        let Some(address) = guard.active.as_ref().cloned() else {
            return vec![];
        };

        let conn_ctx = match ConnectionManagerSelective::get_conn_ctx(&guard, &address) {
            Ok(conn_ctx) => conn_ctx,
            Err(err) => {
                error!("{}", err);
                return vec![];
            },
        };

        if let Some(conn) = conn_ctx.connection.clone() {
            vec![conn]
        } else {
            vec![]
        }
    }

    async fn get_connection_by_address(
        &self,
        address: &str,
    ) -> Result<Arc<AsyncMutex<ElectrumConnection>>, ConnectionManagerErr> {
        debug!("Getting connection for address: {:?}", address);
        let guard = self.state_guard.lock().await;

        let conn_ctx = ConnectionManagerSelective::get_conn_ctx(&guard, address)?;
        conn_ctx
            .connection
            .clone()
            .ok_or_else(|| ConnectionManagerErr::NotConnected(address.to_string()))
    }
}

#[derive(Debug)]
struct ConnectionManagerSelectiveState {
    active: Option<String>,
    connecting: AtomicBool,
    connection_contexts: BTreeMap<String, ElectrumConnCtx>,
    backup_connections: VecDeque<String>,
    primary_connections: VecDeque<String>,
    scripthash_subs: HashMap<String, Arc<AsyncMutex<ElectrumConnection>>>,
}

struct ConnectingAtomicCtx {
    connection_manager: ConnectionManagerSelective,
    mng_spawner: WeakSpawner,
}
impl ConnectingAtomicCtx {
    fn try_new(
        state_guard: &mut MutexGuard<'_, ConnectionManagerSelectiveState>,
        connection_manager: ConnectionManagerSelective,
        mng_spawner: WeakSpawner,
    ) -> Option<ConnectingAtomicCtx> {
        match state_guard
            .connecting
            .compare_exchange(false, true, AtomicOrdering::Acquire, AtomicOrdering::Relaxed)
        {
            Ok(false) => Some(Self {
                connection_manager,
                mng_spawner,
            }),
            Err(true) => None,
            _ => panic!("Failed to connect: unexpected state on compare_exchange connecting state"),
        }
    }
}

impl Drop for ConnectingAtomicCtx {
    fn drop(&mut self) {
        let spawner = self.mng_spawner.clone();
        let connection_manager = self.connection_manager.clone();
        spawner.spawn(async move {
            let state = connection_manager.0.state_guard.lock().await;
            state.connecting.store(false, AtomicOrdering::Relaxed);
        })
    }
}
