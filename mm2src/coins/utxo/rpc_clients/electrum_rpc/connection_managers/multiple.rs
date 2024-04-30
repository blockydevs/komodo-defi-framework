// use async_trait::async_trait;
// use core::time::Duration;
// use futures::lock::Mutex as AsyncMutex;
// use futures::{select, FutureExt};
// use std::collections::HashMap;
// use std::ops::Deref;
// use std::sync::Arc;

// use crate::utxo::ScripthashNotificationSender;
// use common::executor::abortable_queue::{AbortableQueue, WeakSpawner};
// use common::executor::{AbortableSystem, SpawnFuture, Timer};
// use common::log::{debug, error, info, warn};

// use super::connection_manager_common::{ConnectionManagerErr, ConnectionManagerTrait, ElectrumConnCtx,
//                                        DEFAULT_CONN_TIMEOUT_SEC, SUSPEND_TIMEOUT_INIT_SEC};
// use super::{spawn_electrum, ElectrumClientEvent, ElectrumConnection, ElectrumConnectionSettings};

// /// you wanna have a force wake method to wake up suspended servers that were queried specifically
// #[derive(Debug)]
// pub(super) struct ConnectionManagerMultiple {
//     inner_state: AsyncMutex<ConnectionManagerMultipleState>,
//     abortable_system: AbortableQueue,
//     event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
//     scripthash_notification_sender: ScripthashNotificationSender,
// }

// #[derive(Debug)]
// struct ConnectionManagerMultipleState {
//     connection_contexts: Vec<ElectrumConnCtx>,
//     scripthash_subs: HashMap<String, Arc<AsyncMutex<ElectrumConnection>>>,
// }

// #[async_trait]
// impl ConnectionManagerTrait for Arc<ConnectionManagerMultiple> {
//     async fn get_connection(&self) -> Vec<Arc<AsyncMutex<ElectrumConnection>>> {
//         let connections = &self.inner_state.lock().await.connection_contexts;
//         connections
//             .iter()
//             .filter(|conn_ctx| conn_ctx.connection.is_some())
//             .map(|conn_ctx| conn_ctx.connection.as_ref().unwrap().clone())
//             .collect()
//     }

//     async fn get_connection_by_address(
//         &self,
//         address: &str,
//     ) -> Result<Arc<AsyncMutex<ElectrumConnection>>, ConnectionManagerErr> {
//         let inner = self.inner_state.lock().await;
//         let (_, conn_ctx) = inner.get_connection_ctx(address)?;
//         conn_ctx
//             .connection
//             .as_ref()
//             .cloned()
//             .ok_or_else(|| ConnectionManagerErr::NotConnected(address.to_string()))
//     }

//     async fn connect(&self) -> Result<(), ConnectionManagerErr> {
//         let mut inner = self.inner_state.lock().await;

//         if inner.connection_contexts.is_empty() {
//             return Err(ConnectionManagerErr::SettingsNotSet);
//         }

//         for context in &mut inner.connection_contexts {
//             if context.connection.is_some() {
//                 let address = &context.conn_settings.url;
//                 warn!("An attempt to connect over an existing one: {}", address);
//                 continue;
//             }
//             let conn_settings = context.conn_settings.clone();
//             let weak_spawner = context.abortable_system.weak_spawner();
//             let self_clone = self.clone();
//             self.abortable_system.weak_spawner().spawn(async move {
//                 let _ = self_clone.connect_to(&conn_settings, weak_spawner).await;
//             });
//         }

//         Ok(())
//     }

//     async fn is_connected(&self) -> bool {
//         let inner = self.inner_state.lock().await;

//         for conn_ctx in inner.connection_contexts.iter() {
//             if let Some(ref connection) = conn_ctx.connection {
//                 if connection.lock().await.is_connected().await {
//                     return true;
//                 }
//             }
//         }

//         false
//     }

//     async fn remove_server(&self, address: &str) -> Result<(), ConnectionManagerErr> {
//         debug!("Remove electrum server: {}", address);
//         let mut inner = self.inner_state.lock().await;
//         let (i, _) = inner.get_connection_ctx(address)?;
//         let conn_ctx = inner.connection_contexts.remove(i);
//         conn_ctx
//             .abortable_system
//             .abort_all()
//             .map_err(|err| ConnectionManagerErr::FailedAbort(address.to_string(), err))?;
//         Ok(())
//     }

//     async fn rotate_servers(&self, no_of_rotations: usize) {
//         debug!("Rotate servers: {}", no_of_rotations);
//         let mut inner = self.inner_state.lock().await;
//         inner.connection_contexts.rotate_left(no_of_rotations);
//     }

//     async fn is_connections_pool_empty(&self) -> bool { self.inner_state.lock().await.connection_contexts.is_empty() }

//     async fn on_disconnected(&self, address: &str) {
//         info!(
//             "electrum_connection_manager disconnected from: {}, it will be suspended and trying to reconnect",
//             address
//         );
//         let self_copy = self.clone();
//         // check if any scripthash is subscribed to this addr and remove.
//         self.remove_subscription_by_addr(address).await;

//         let address = address.to_owned();
//         self.abortable_system.weak_spawner().spawn(async move {
//             if let Err(err) = self_copy.clone().suspend_server(&address).await {
//                 error!("Failed to suspend server: {}, error: {}", address, err);
//             }
//         });
//     }

//     async fn add_subscription(&self, script_hash: &str) {
//         if let Some(connected) = self.get_connected().await {
//             let mut subs = self.inner_state.lock().await;
//             subs.scripthash_subs.insert(script_hash.to_string(), connected);
//         };
//     }

//     async fn check_script_hash_subscription(&self, script_hash: &str) -> bool {
//         let mut inner = self.inner_state.lock().await;
//         // Find script_hash connection/subscription
//         if let Some(connection) = inner.scripthash_subs.clone().get(script_hash) {
//             let connection = connection.lock().await;
//             // return true if there's an active connection.
//             if connection.is_connected().await {
//                 return true;
//             }
//             //  Proceed to remove subscription if found but not connected/no connection..
//             inner.scripthash_subs.remove(script_hash);
//         };

//         false
//     }

//     /// remove scripthash subscription from list by server addr
//     async fn remove_subscription_by_addr(&self, server_addr: &str) {
//         let mut inner = self.inner_state.lock().await;
//         // remove server from scripthash subscription list
//         for (script, conn) in inner.scripthash_subs.clone() {
//             if conn.lock().await.addr == server_addr {
//                 inner.scripthash_subs.remove(&script);
//             }
//         }
//     }
// }

// impl ConnectionManagerMultiple {
//     fn try_new_arc(
//         servers: Vec<ElectrumConnectionSettings>,
//         abortable_system: AbortableQueue,
//         event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
//         scripthash_notification_sender: ScripthashNotificationSender,
//     ) -> Result<Arc<Self>, String> {
//         let mut connections = vec![];
//         for conn_settings in servers {
//             let subsystem = abortable_system.create_subsystem().map_err(|err| {
//                 ERRL!(
//                     "Failed to create abortable subsystem for conn: {}, error: {}",
//                     conn_settings.url,
//                     err
//                 )
//             })?;

//             connections.push(ElectrumConnCtx {
//                 conn_settings,
//                 abortable_system: subsystem,
//                 suspend_timeout_sec: SUSPEND_TIMEOUT_INIT_SEC,
//                 connection: None,
//             });
//         }

//         Ok(Arc::new(ConnectionManagerMultiple {
//             abortable_system,
//             event_sender,
//             inner_state: AsyncMutex::new(ConnectionManagerMultipleState {
//                 connection_contexts: connections,
//                 scripthash_subs: HashMap::new(),
//             }),
//             scripthash_notification_sender,
//         }))
//     }

//     async fn suspend_server(self: Arc<ConnectionManagerMultiple>, address: &str) -> Result<(), ConnectionManagerErr> {
//         debug!(
//             "About to suspend connection to addr: {}, inner: {:?}",
//             address, self.inner_state
//         );
//         let mut inner = self.inner_state.lock().await;

//         inner.reset_connection_context(address, self.abortable_system.create_subsystem().unwrap())?;

//         let suspend_timeout_sec = inner.get_suspend_timeout(address)?;
//         inner.duplicate_suspend_timeout(address)?;
//         drop(inner);

//         self.spawn_resume_server(address, suspend_timeout_sec);
//         debug!("Suspend future spawned");
//         Ok(())
//     }

//     // workaround to avoid the cycle detected compilation error that blocks recursive async calls
//     fn spawn_resume_server(self: Arc<ConnectionManagerMultiple>, address: &str, suspend_timeout_sec: u64) {
//         let spawner = self.abortable_system.weak_spawner();
//         let address = address.to_owned();
//         spawner.spawn(Box::new(
//             async move {
//                 debug!("Suspend server: {}, for: {} seconds", address, suspend_timeout_sec);
//                 Timer::sleep(suspend_timeout_sec as f64).await;
//                 let _ = self.resume_server(&address).await;
//             }
//             .boxed(),
//         ));
//     }

//     async fn resume_server(self: Arc<ConnectionManagerMultiple>, address: &str) -> Result<(), ConnectionManagerErr> {
//         debug!("Resume address: {}", address);
//         let inner = self.inner_state.lock().await;

//         let (_, conn_ctx) = inner.get_connection_ctx(address)?;
//         let conn_settings = conn_ctx.conn_settings.clone();
//         let conn_spawner = conn_ctx.abortable_system.weak_spawner();
//         drop(inner);

//         if let Err(err) = self.clone().connect_to(&conn_settings, conn_spawner).await {
//             error!("Failed to resume: {}", err);
//             self.suspend_server(address).await?;
//         }
//         Ok(())
//     }

//     async fn connect_to(
//         self: Arc<ConnectionManagerMultiple>,
//         conn_settings: &ElectrumConnectionSettings,
//         weak_spawner: WeakSpawner,
//     ) -> Result<(), ConnectionManagerErr> {
//         let (conn, mut conn_ready_receiver) = spawn_electrum(
//             conn_settings,
//             weak_spawner.clone(),
//             self.event_sender.clone(),
//             &self.scripthash_notification_sender,
//         )?;
//         self.inner_state.lock().await.register_connection(conn)?;
//         let timeout_sec = conn_settings.timeout_sec.unwrap_or(DEFAULT_CONN_TIMEOUT_SEC);
//         let address = conn_settings.url.clone();
//         select! {
//             _ = async_std::task::sleep(Duration::from_secs(timeout_sec)).fuse() => {
//                 self
//                 .suspend_server(&address)
//                 .await
//             },
//             _ = conn_ready_receiver => {
//                 self.inner_state.lock().await.reset_suspend_timeout(&address)
//             }
//         }
//     }

//     async fn get_connected(&self) -> Option<Arc<AsyncMutex<ElectrumConnection>>> {
//         let subs = self.inner_state.lock().await;
//         for connection in subs.connection_contexts.iter() {
//             if let Some(connection) = &connection.connection {
//                 let conn = connection.lock().await;
//                 if conn.is_connected().await {
//                     return Some(connection.clone());
//                 }
//             }
//         }

//         None
//     }
// }

// impl ConnectionManagerMultipleState {
//     fn register_connection(&mut self, conn: ElectrumConnection) -> Result<(), ConnectionManagerErr> {
//         let (_, conn_ctx) = self.get_connection_ctx_mut(&conn.addr)?;
//         conn_ctx.connection.replace(Arc::new(AsyncMutex::new(conn)));
//         Ok(())
//     }

//     fn reset_connection_context(
//         &mut self,
//         address: &str,
//         abortable_system: AbortableQueue,
//     ) -> Result<(), ConnectionManagerErr> {
//         debug!("Reset connection context for: {}", address);
//         let (_, conn_ctx) = self.get_connection_ctx_mut(address)?;
//         conn_ctx
//             .abortable_system
//             .abort_all()
//             .map_err(|err| ConnectionManagerErr::FailedAbort(address.to_string(), err))?;
//         conn_ctx.connection.take();
//         conn_ctx.abortable_system = abortable_system;
//         Ok(())
//     }

//     fn get_suspend_timeout(&mut self, address: &str) -> Result<u64, ConnectionManagerErr> {
//         self.get_connection_ctx(address)
//             .map(|(_, conn_ctx)| conn_ctx.suspend_timeout_sec)
//     }

//     fn duplicate_suspend_timeout(&mut self, address: &str) -> Result<(), ConnectionManagerErr> {
//         self.set_suspend_timeout(address, |origin| origin.checked_mul(2).unwrap_or(u64::MAX))
//     }

//     fn reset_suspend_timeout(&mut self, address: &str) -> Result<(), ConnectionManagerErr> {
//         self.set_suspend_timeout(address, |_| SUSPEND_TIMEOUT_INIT_SEC)
//     }

//     fn set_suspend_timeout<F: Fn(u64) -> u64>(&mut self, address: &str, method: F) -> Result<(), ConnectionManagerErr> {
//         let (_, conn_ctx) = self.get_connection_ctx_mut(address)?;
//         let suspend_timeout = &mut conn_ctx.suspend_timeout_sec;
//         let new_value = method(*suspend_timeout);
//         debug!(
//             "Set suspend timeout for address: {} - from: {} to the value: {}",
//             address, suspend_timeout, new_value
//         );
//         *suspend_timeout = new_value;
//         Ok(())
//     }

//     fn get_connection_ctx(&self, address: &str) -> Result<(usize, &ElectrumConnCtx), ConnectionManagerErr> {
//         self.connection_contexts
//             .iter()
//             .enumerate()
//             .find(|(_, c)| c.conn_settings.url == address)
//             .ok_or_else(|| ConnectionManagerErr::UnknownAddress(address.to_string()))
//     }

//     fn get_connection_ctx_mut(&mut self, address: &str) -> Result<(usize, &mut ElectrumConnCtx), ConnectionManagerErr> {
//         self.connection_contexts
//             .iter_mut()
//             .enumerate()
//             .find(|(_, ctx)| ctx.conn_settings.url == address)
//             .ok_or_else(|| ConnectionManagerErr::UnknownAddress(address.to_string()))
//     }
// }
