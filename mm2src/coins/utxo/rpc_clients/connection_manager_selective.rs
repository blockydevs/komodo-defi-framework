use async_trait::async_trait;
use core::time::Duration;
use futures::lock::Mutex as AsyncMutex;
use futures::{select, FutureExt};
use std::collections::{HashMap, VecDeque};
use std::ops::Deref;
use std::sync::Arc;

use crate::utxo::ScripthashNotificationSender;
use common::executor::abortable_queue::{AbortableQueue, WeakSpawner};
use common::executor::{AbortableSystem, SpawnFuture, Timer};
use common::log::{debug, error, info, warn};

use super::connection_manager_common::{ConnectionManagerErr, ConnectionManagerTrait, ElectrumConnCtx,
                                       DEFAULT_CONN_TIMEOUT_SEC, SUSPEND_TIMEOUT_INIT_SEC};
use super::{spawn_electrum, ElectrumClientEvent, ElectrumConnSettings, ElectrumConnection};
use mm2_rpc::data::legacy::Priority;

#[derive(Clone, Debug)]
pub(super) struct ConnectionManagerSelective(Arc<ConnectionManagerSelectiveImpl>);

#[derive(Debug)]
struct ConnectionManagerSelectiveImpl {
    inner_state: AsyncMutex<ConnectionManagerSelectiveState>,
    abortable_system: AbortableQueue,
    event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
    scripthash_notification_sender: ScripthashNotificationSender,
}

#[derive(Debug)]
struct ConnectionManagerSelectiveState {
    active: Option<String>,
    connection_contexts: HashMap<String, ElectrumConnCtx>,
    backup_connections: VecDeque<String>,
    primary_connections: VecDeque<String>,
    scripthash_subs: HashMap<String, Arc<AsyncMutex<ElectrumConnection>>>,
}

impl Deref for ConnectionManagerSelective {
    type Target = dyn ConnectionManagerTrait + Send + Sync;

    fn deref(&self) -> &Self::Target { &self.0 }
}

impl ConnectionManagerSelective {
    pub(super) fn try_new(
        servers: Vec<ElectrumConnSettings>,
        abortable_system: AbortableQueue,
        event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
        scripthash_notification_sender: ScripthashNotificationSender,
    ) -> Result<Self, String> {
        let inner = ConnectionManagerSelectiveImpl::try_new(
            servers,
            abortable_system,
            event_sender,
            scripthash_notification_sender,
        )?;
        Ok(ConnectionManagerSelective(Arc::new(inner)))
    }
}

#[async_trait]
impl ConnectionManagerTrait for Arc<ConnectionManagerSelectiveImpl> {
    async fn get_connection(&self) -> Vec<Arc<AsyncMutex<ElectrumConnection>>> {
        debug!("Getting available connection");
        let inner = self.inner_state.lock().await;
        let Some(address) = inner.active.as_ref() else {
            return vec![];
        };

        let conn_ctx = match inner.get_connection_ctx(address) {
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
        let inner = self.inner_state.lock().await;

        let conn_ctx = inner.get_connection_ctx(address)?;
        conn_ctx
            .connection
            .clone()
            .ok_or_else(|| ConnectionManagerErr::NotConnected(address.to_string()))
    }

    async fn connect(&self) -> Result<(), ConnectionManagerErr> {
        while let Some((conn_settings, weak_spawner)) = self.fetch_conn_settings().await {
            debug!("Got conn_settings to connect to: {:?}", conn_settings);
            let address = conn_settings.url.clone();
            match self
                .connect_to(
                    conn_settings,
                    weak_spawner,
                    self.event_sender.clone(),
                    &self.scripthash_notification_sender,
                )
                .await
            {
                Ok(_) => {
                    self.inner_state.lock().await.set_active_connection(address)?;
                    break;
                },
                Err(_) => {
                    self.clone().suspend_server(address.clone()).await?;
                },
            };
        }
        Ok(())
    }

    async fn is_connected(&self) -> bool {
        let inner = self.inner_state.lock().await;

        if let Some(active) = &inner.active {
            if let Ok(ctx) = inner.get_connection_ctx(active) {
                if let Some(ref connection) = ctx.connection {
                    return connection.lock().await.is_connected().await;
                }
            }
        }

        false
    }

    async fn remove_server(&self, address: &str) -> Result<(), ConnectionManagerErr> {
        debug!("Remove server: {}", address);
        let mut inner = self.inner_state.lock().await;
        let conn_ctx = inner
            .connection_contexts
            .remove(address)
            .ok_or_else(|| ConnectionManagerErr::UnknownAddress(address.to_string()))?;

        match conn_ctx.conn_settings.priority {
            Priority::Primary => inner.primary_connections.pop_front(),
            Priority::Secondary => inner.backup_connections.pop_front(),
        };
        if let Some(active) = inner.active.as_ref() {
            if active == address {
                inner.active.take();
            }
        }
        Ok(())
    }

    async fn rotate_servers(&self, _no_of_rotations: usize) {
        // TODO: Change the active server
    }

    async fn is_connections_pool_empty(&self) -> bool { self.inner_state.lock().await.connection_contexts.is_empty() }

    async fn on_disconnected(&self, address: &str) {
        info!(
            "electrum_connection_manager disconnected from: {}, it will be suspended and trying to reconnect",
            address
        );
        let self_copy = self.clone();
        let address = address.to_string();
        // check if any scripthash is subscribed to this addr and remove.
        self.remove_subscription_by_addr(&address).await;
        self.abortable_system.weak_spawner().spawn(async move {
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
        let mut inner = self.inner_state.lock().await;
        if let Some(ref active) = inner.active {
            if let Some(conn) = inner.get_connection_ctx(active).unwrap().connection.clone() {
                inner.scripthash_subs.insert(script_hash.to_string(), conn);
            };
        };
    }

    async fn check_script_hash_subscription(&self, script_hash: &str) -> bool {
        let mut inner = self.inner_state.lock().await;
        // Find script_hash connection/subscription
        if let Some(connection) = inner.scripthash_subs.clone().get(script_hash) {
            let connection = connection.lock().await;
            if connection.is_connected().await {
                return true;
            }

            //  Proceed to remove subscription if found but not connected/no connection..
            inner.scripthash_subs.remove(script_hash);
        };

        false
    }

    async fn remove_subscription_by_addr(&self, server_addr: &str) {
        let mut inner = self.inner_state.lock().await;
        // remove server from scripthash subscription list
        for (script, conn) in inner.scripthash_subs.clone() {
            if conn.lock().await.addr == server_addr {
                inner.scripthash_subs.remove(&script);
            }
        }
    }
}

impl ConnectionManagerSelectiveImpl {
    fn try_new(
        servers: Vec<ElectrumConnSettings>,
        abortable_system: AbortableQueue,
        event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
        scripthash_notification_sender: ScripthashNotificationSender,
    ) -> Result<Self, String> {
        let mut primary_connections = VecDeque::<String>::new();
        let mut backup_connections = VecDeque::<String>::new();
        let mut connection_contexts = HashMap::new();
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
            inner_state: AsyncMutex::new(ConnectionManagerSelectiveState {
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

    async fn fetch_conn_settings(&self) -> Option<(ElectrumConnSettings, WeakSpawner)> {
        let inner = self.inner_state.lock().await;
        if inner.active.is_some() {
            warn!("Skip connecting, already connected");
            return None;
        }

        debug!("Primary electrum nodes to connect: {:?}", inner.primary_connections);
        debug!("Backup electrum nodes to connect: {:?}", inner.backup_connections);
        let mut iter = inner.primary_connections.iter().chain(inner.backup_connections.iter());
        let addr = iter.next()?.clone();
        if let Ok(conn_ctx) = inner.get_connection_ctx(&addr) {
            Some((conn_ctx.conn_settings.clone(), conn_ctx.abortable_system.weak_spawner()))
        } else {
            warn!("Failed to connect, no connection settings found");
            None
        }
    }

    async fn suspend_server(
        self: Arc<ConnectionManagerSelectiveImpl>,
        address: String,
    ) -> Result<(), ConnectionManagerErr> {
        debug!(
            "About to suspend connection to addr: {}, inner: {:?}",
            address, self.inner_state
        );
        let mut inner = self.inner_state.lock().await;
        if let Some(ref active) = inner.active {
            if *active == address {
                inner.active.take();
            }
        }

        match inner.get_connection_ctx(&address)?.conn_settings.priority {
            Priority::Primary => {
                inner.primary_connections.pop_front();
            },
            Priority::Secondary => {
                inner.backup_connections.pop_front();
            },
        };

        inner.reset_connection_context(&address, self.abortable_system.create_subsystem().unwrap())?;

        let suspend_timeout_sec = inner.get_suspend_timeout(&address)?;
        inner.duplicate_suspend_timeout(&address)?;
        drop(inner);

        self.spawn_resume_server(address, suspend_timeout_sec);
        debug!("Suspend future spawned");
        Ok(())
    }

    // workaround to avoid the cycle detected compilation error that blocks recursive async calls
    fn spawn_resume_server(self: Arc<ConnectionManagerSelectiveImpl>, address: String, suspend_timeout_sec: u64) {
        let spawner = self.abortable_system.weak_spawner();
        spawner.spawn(Box::new(
            async move {
                debug!("Suspend server: {}, for: {} seconds", address, suspend_timeout_sec);
                Timer::sleep(suspend_timeout_sec as f64).await;
                let _ = self.resume_server(address).await;
            }
            .boxed(),
        ));
    }

    async fn resume_server(
        self: Arc<ConnectionManagerSelectiveImpl>,
        address: String,
    ) -> Result<(), ConnectionManagerErr> {
        debug!("Resume address: {}", address);
        let mut inner = self.inner_state.lock().await;
        let priority = inner.get_connection_ctx(&address)?.conn_settings.priority.clone();
        match priority {
            Priority::Primary => inner.primary_connections.push_back(address.clone()),
            Priority::Secondary => inner.backup_connections.push_back(address.clone()),
        }

        if let Some(active) = inner.active.clone() {
            let conn_ctx = inner.get_connection_ctx(&address)?;
            let active_ctx = inner.get_connection_ctx(&active)?;
            let active_priority = &active_ctx.conn_settings.priority;
            if let (Priority::Secondary, Priority::Primary) = (active_priority, priority) {
                let conn_settings = conn_ctx.conn_settings.clone();
                let conn_spawner = conn_ctx.abortable_system.weak_spawner();
                drop(inner);
                if let Err(err) = self
                    .connect_to(
                        conn_settings,
                        conn_spawner,
                        self.event_sender.clone(),
                        &self.scripthash_notification_sender,
                    )
                    .await
                {
                    error!("Failed to resume: {}", err);
                    self.suspend_server(address.clone()).await?;
                } else {
                    let mut inner = self.inner_state.lock().await;
                    inner.reset_connection_context(&active, self.abortable_system.create_subsystem().unwrap())?;
                    inner.set_active_connection(address.clone())?;
                }
            }
        } else {
            drop(inner);
            let _ = self.connect().await;
        };
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
        self.inner_state.lock().await.register_connection(conn)?;
        let timeout_sec = conn_settings.timeout_sec.unwrap_or(DEFAULT_CONN_TIMEOUT_SEC);

        select! {
            _ = async_std::task::sleep(Duration::from_secs(timeout_sec)).fuse() => {
                warn!("Failed to connect to: {}, timed out", conn_settings.url);
                Err(ConnectionManagerErr::ConnectingError(conn_settings.url.clone(), format!("Timed out: {}", timeout_sec)))
            },
            _ = conn_ready_receiver => Ok(()) // TODO: handle cancelled
        }
    }
}

impl ConnectionManagerSelectiveState {
    fn get_connection_ctx(&self, address: &str) -> Result<&ElectrumConnCtx, ConnectionManagerErr> {
        self.connection_contexts
            .get(address)
            .ok_or_else(|| ConnectionManagerErr::UnknownAddress(address.to_string()))
    }

    fn get_connection_ctx_mut(&mut self, address: &str) -> Result<&mut ElectrumConnCtx, ConnectionManagerErr> {
        self.connection_contexts
            .get_mut(address)
            .ok_or_else(|| ConnectionManagerErr::UnknownAddress(address.to_string()))
    }

    fn set_active_connection(&mut self, address: String) -> Result<(), ConnectionManagerErr> {
        self.reset_suspend_timeout(&address)?;
        self.active.replace(address);
        Ok(())
    }

    fn reset_connection_context(
        &mut self,
        address: &str,
        abortable_system: AbortableQueue,
    ) -> Result<(), ConnectionManagerErr> {
        debug!("Reset connection context for: {}", address);

        let conn_ctx = self.get_connection_ctx_mut(address)?;
        conn_ctx
            .abortable_system
            .abort_all()
            .map_err(|err| ConnectionManagerErr::FailedAbort(address.to_string(), err))?;
        conn_ctx.connection.take();
        conn_ctx.abortable_system = abortable_system;
        Ok(())
    }

    fn register_connection(&mut self, conn: ElectrumConnection) -> Result<(), ConnectionManagerErr> {
        let conn_ctx = self.get_connection_ctx_mut(&conn.addr)?;
        conn_ctx.connection.replace(Arc::new(AsyncMutex::new(conn)));
        Ok(())
    }

    fn get_suspend_timeout(&self, address: &str) -> Result<u64, ConnectionManagerErr> {
        self.get_connection_ctx(address).map(|ctx| ctx.suspend_timeout_sec)
    }

    fn duplicate_suspend_timeout(&mut self, address: &str) -> Result<(), ConnectionManagerErr> {
        self.set_suspend_timeout(address, |origin| origin.checked_mul(2).unwrap_or(u64::MAX))
    }

    fn reset_suspend_timeout(&mut self, address: &str) -> Result<(), ConnectionManagerErr> {
        // TODO: We should probably reset the timeout to the original timeout used for this electrum server?
        self.set_suspend_timeout(address, |_| SUSPEND_TIMEOUT_INIT_SEC)
    }

    fn set_suspend_timeout<F: Fn(u64) -> u64>(&mut self, address: &str, method: F) -> Result<(), ConnectionManagerErr> {
        let conn_ctx = self.get_connection_ctx_mut(address)?;
        let suspend_timeout = &mut conn_ctx.suspend_timeout_sec;
        let new_value = method(*suspend_timeout);
        debug!(
            "Set supsend timeout for address: {} - from: {} to the value: {}",
            address, suspend_timeout, new_value
        );
        *suspend_timeout = new_value;
        Ok(())
    }
}
