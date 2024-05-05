use sha2::{Digest, Sha256};

mod client;
mod connection;
mod connection_managers;
mod constants;
mod event_handlers;
mod rpc_responses;
mod tcp_stream;

pub use client::{ElectrumClient, ElectrumClientImpl, ElectrumClientSettings};
pub use connection::{ElectrumConnection, ElectrumConnectionSettings};
pub use constants::*;

#[inline]
pub fn electrum_script_hash(script: &[u8]) -> Vec<u8> {
    let mut sha = Sha256::new();
    sha.update(script);
    sha.finalize().to_vec().into_iter().rev().collect()
}

// #[cfg(target_arch = "wasm32")]
// #[allow(clippy::too_many_arguments)]
// async fn establish_connection_loop<Spawner: SpawnFuture>(
//     connection: Arc<ElectrumConnection>,
//     client: ElectrumClient,
// ) -> Result<(), ConnectionManagerErr> {
//     use mm2_net::wasm::wasm_ws::ws_transport;
//     use std::sync::atomic::AtomicUsize;
//     lazy_static! {
//         static ref CONN_IDX: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
//     }

//     let mut addr = connection.address().to_string();
//     let uri: Uri = connection
//         .address()
//         .parse()
//         .map_err(|e| log_and_return!(Irrecoverable, format!("Failed to parse the address: {e:?}")))?;

//     if uri.scheme().is_some() {
//         log_and_return!(
//             Irrecoverable,
//             "There has not to be a scheme in the url. \
//             'ws://' scheme is used by default. \
//             Consider using 'protocol: \"WSS\"' in the electrum request to switch to the 'wss://' scheme."
//         );
//     }

//     match connection.settings.protocol {
//         ElectrumProtocol::WS => {
//             addr.insert_str(0, "ws://");
//         },
//         ElectrumProtocol::WSS => {
//             addr.insert_str(0, "wss://");
//         },
//         ElectrumProtocol::TCP | ElectrumProtocol::SSL => {
//             log_and_return!(
//                 Irrecoverable,
//                 "'TCP' and 'SSL' are not supported in a browser. Please use 'WS' or 'WSS' protocols"
//             );
//         },
//     };

//     let conn_idx = CONN_IDX.fetch_add(1, AtomicOrdering::Relaxed);
//     let (mut transport_tx, mut transport_rx) = handle_connect_err!(ws_transport(conn_idx, &addr, &spawner).await, addr);

//     info!("Electrum client connected to {}", addr);
//     event_handlers.on_connected(addr).await;

//     let last_response = Arc::new(AtomicU64::new(now_ms()));
//     let no_connection_timeout = {
//         let last_response = last_response.clone();
//         async move {
//             loop {
//                 Timer::sleep(ELECTRUM_TIMEOUT_SEC as f64).await;
//                 let last_sec = (last_response.load(AtomicOrdering::Relaxed) / 1000) as f64;
//                 if now_float() - last_sec > ELECTRUM_TIMEOUT_SEC as f64 {
//                     warn!(
//                         "Didn't receive any data since {}. Shutting down the connection.",
//                         last_sec as i64
//                     );
//                     break;
//                 }
//             }
//         }
//     };
//     let mut no_connection_timeout = Box::pin(no_connection_timeout).fuse();

//     let (outgoing_tx, outgoing_rx) = mpsc::channel(0);
//     *connection_tx.lock().await = Some(outgoing_tx);

//     let incoming_fut = {
//         let addr = addr.clone();
//         let responses = responses.clone();
//         let scripthash_notification_sender = scripthash_notification_sender.clone();
//         let event_sender = event_sender.clone();
//         async move {
//             while let Some(incoming_res) = transport_rx.next().await {
//                 last_response.store(now_ms(), AtomicOrdering::Relaxed);
//                 match incoming_res {
//                     Ok(incoming_json) => {
//                         process_electrum_response(incoming_json, &event_handlers).await;
//                     },
//                     Err(e) => {
//                         error!("{} error: {:?}", addr, e);
//                     },
//                 }
//             }
//         }
//     };
//     let mut incoming_fut = Box::pin(incoming_fut).fuse();

//     let outgoing_fut = {
//         let addr = addr.clone();
//         let mut outgoing_rx = rx_to_stream(outgoing_rx).compat();
//         let event_sender = event_sender.clone();
//         async move {
//             while let Some(Ok(data)) = outgoing_rx.next().await {
//                 let raw_json: Json = match json::from_slice(&data) {
//                     Ok(js) => js,
//                     Err(e) => {
//                         error!("Error {} deserializing the outgoing data: {:?}", e, data);
//                         continue;
//                     },
//                 };
//                 // measure the length of each sent packet
//                 handle_connect_err!(
//                     event_sender.unbounded_send(ElectrumClientEvent::OutgoingRequest { data_len: data.len() }),
//                     addr
//                 );

//                 if let Err(e) = transport_tx.send(raw_json).await {
//                     error!("Error sending to {}: {:?}", addr, e);
//                 }
//             }
//         }
//     };
//     let mut outgoing_fut = Box::pin(outgoing_fut).fuse();

//     select! {
//         _ = no_connection_timeout => (),
//         _ = incoming_fut => (),
//         _ = outgoing_fut => (),
//     }

//     info!("{} connection dropped", addr);
//     connection.disconnect();
//     event_handlers.on_disconnected(addr).await;
// }

// /// Attempts to process the request (parse url, etc), build up the config and create new electrum connection
// /// The function takes `abortable_system` that will be used to spawn Electrum's related futures.
// #[cfg(target_arch = "wasm32")]
// fn spawn_electrum(
//     conn_settings: &ElectrumConnectionSettings,
//     spawner: WeakSpawner,
//     event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
//     scripthash_notification_sender: &ScripthashNotificationSender,
// ) -> Result<(ElectrumConnection, async_oneshot::Receiver<()>), ConnectionManagerErr> {
//     Ok(electrum_connect(
//         conn_settings.url.clone(),
//         config,
//         spawner,
//         event_sender,
//         scripthash_notification_sender,
//     ))
// }
