use super::client::ElectrumClient;
use super::constants::{CUTOFF_TIMEOUT, DEFAULT_CONNECTION_ESTABLISHMENT_TIMEOUT};

use crate::{RpcTransportEventHandler, SharableRpcTransportEventHandler};
use common::custom_futures::timeout::FutureTimerExt;
use common::executor::{abortable_queue::AbortableQueue, abortable_queue::WeakSpawner, AbortableSystem, SpawnFuture,
                       Timer};
use common::jsonrpc_client::{JsonRpcBatchResponse, JsonRpcErrorType, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
                             JsonRpcResponseEnum};
use common::log::{error, info};
use common::{now_float, now_ms, OrdRange};
use mm2_rpc::data::legacy::{ElectrumProtocol, Priority};

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;

use futures::channel::oneshot as async_oneshot;
use futures::compat::{Future01CompatExt, Stream01CompatExt};
use futures::future::FutureExt;
use futures::lock::Mutex as AsyncMutex;
use futures::select;
use futures::stream::StreamExt;
use futures01::sync::mpsc;
use futures01::{Sink, Stream};
use http::Uri;
use instant::Instant;
use serde::Serialize;
use serde_json::{self as json, Value as Json};

cfg_native! {
    use super::tcp_stream::*;

    use std::convert::TryFrom;
    use std::net::ToSocketAddrs;

    use futures::future::{Either, TryFutureExt};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;
    use tokio_rustls::{TlsConnector};
    use tokio_rustls::webpki::DnsNameRef;
    use rustls::{ServerName};
}

pub type JsonRpcPendingRequests = HashMap<JsonRpcId, async_oneshot::Sender<JsonRpcResponseEnum>>;

macro_rules! disconnect_and_return {
    ($typ:tt, $err:expr, $conn:expr, $handlers:expr) => {{
        let err = ElectrumConnectionErr::$typ(format!("{:?}", $err));
        disconnect_and_return!(err, $conn, $handlers);
    }};
    ($err:expr, $conn:expr, $handlers:expr) => {{
        // Disconnect the connection.
        $conn.disconnect(Some($err.clone())).await;
        // Inform the event handlers of the disconnection.
        // Note that it's important to inform the handler after disconnecting and not before.
        // This way we are sure that the last signal sent to the handler is the disconnection signal.
        $handlers.on_disconnected(&$conn.address()).ok();
        error!("{} {:?}", $conn.address(), $err);
        return Err($err);
    }};
}

macro_rules! disconnect_and_return_if_err {
    ($ex:expr, $typ:tt, $conn:expr, $handlers:expr) => {{
        match $ex {
            Ok(res) => res,
            Err(e) => {
                disconnect_and_return!($typ, e, $conn, $handlers);
            },
        }
    }};
}

/// Helper function casting mpsc::Receiver as Stream.
fn rx_to_stream(rx: mpsc::Receiver<Vec<u8>>) -> impl Stream<Item = Vec<u8>, Error = io::Error> {
    rx.map_err(|_| panic!("errors not possible on rx"))
}

/// Electrum request RPC representation
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ElectrumConnectionSettings {
    pub url: String,
    #[serde(default)]
    pub protocol: ElectrumProtocol,
    #[serde(default)]
    pub disable_cert_verification: bool,
    #[serde(default)]
    pub priority: Priority,
    pub timeout_sec: Option<f64>,
}

#[derive(Clone, Debug)]
pub enum ElectrumConnectionErr {
    Timeout(f64),
    Temporary(String),
    Irrecoverable(String),
    VersionMismatch(OrdRange<f32>, f32),
}

impl ElectrumConnectionErr {
    pub fn is_recoverable(&self) -> bool {
        match self {
            ElectrumConnectionErr::Irrecoverable(_) => false,
            ElectrumConnectionErr::Timeout(_)
            | ElectrumConnectionErr::Temporary(_)
            // We won't consider version mismatch as irrecoverable since assuming that
            // a server's version can change over time.
            | ElectrumConnectionErr::VersionMismatch(_, _) => true,
        }
    }
}

/// Represents the active Electrum connection to selected address
#[derive(Debug)]
pub struct ElectrumConnection {
    /// The client connected to this SocketAddr
    settings: ElectrumConnectionSettings,
    /// The Sender forwarding requests to writing part of underlying stream
    tx: AsyncMutex<Option<mpsc::Sender<Vec<u8>>>>,
    /// Responses are stored here
    responses: AsyncMutex<JsonRpcPendingRequests>,
    /// Selected protocol version. The value is initialized after the server.version RPC call.
    protocol_version: AsyncMutex<Option<f32>>,
    /// Why was the connection disconnected the last time?
    last_error: AsyncMutex<Option<ElectrumConnectionErr>>,
    /// An abortable system for connection specific tasks to run on.
    abortable_system: AbortableQueue,
}

impl ElectrumConnection {
    pub fn new(settings: ElectrumConnectionSettings, abortable_system: AbortableQueue) -> Self {
        ElectrumConnection {
            settings,
            tx: AsyncMutex::new(None),
            responses: AsyncMutex::new(JsonRpcPendingRequests::new()),
            protocol_version: AsyncMutex::new(None),
            last_error: AsyncMutex::new(None),
            abortable_system,
        }
    }

    pub fn address(&self) -> &str { &self.settings.url }

    pub fn is_primary(&self) -> bool { matches!(self.settings.priority, Priority::Primary) }

    pub fn is_secondary(&self) -> bool { matches!(self.settings.priority, Priority::Secondary) }

    fn weak_spawner(&self) -> WeakSpawner { self.abortable_system.weak_spawner() }

    pub async fn is_connected(&self) -> bool { self.tx.lock().await.is_some() }

    async fn set_protocol_version(&self, version: f32) { self.protocol_version.lock().await.replace(version); }

    async fn clear_protocol_version(&self) { self.protocol_version.lock().await.take(); }

    pub async fn protocol_version(&self) -> Option<f32> { *self.protocol_version.lock().await }

    async fn set_last_error(&self, reason: ElectrumConnectionErr) { self.last_error.lock().await.replace(reason); }

    async fn clear_last_error(&self) { self.last_error.lock().await.take(); }

    pub async fn last_error(&self) -> Option<ElectrumConnectionErr> { self.last_error.lock().await.clone() }

    /// Returns whether the connection is usable or not (irrecoverably disconnected).
    ///
    /// It is usable if:
    ///     1- It is not errored.
    ///     2- Has errored but the error is recoverable.
    pub async fn usable(&self) -> bool { self.last_error().await.map(|le| le.is_recoverable()).unwrap_or(true) }

    /// Try to connect to the electrum server.
    ///
    /// This method will try to connect to the electrum server. It is atomic, meaning that if it was called
    /// multiple times by accident, only the first connection will hold (a `true` is then returned).
    /// Other connections won't overwrite the first connection (and a `false` will then be return).
    async fn try_connect(&self, tx: mpsc::Sender<Vec<u8>>) -> bool {
        let mut tx_guard = self.tx.lock().await;
        if tx_guard.is_some() {
            // We are already connected to the server. Tell the caller that we won't accept connections until we disconnect.
            return false;
        }
        tx_guard.replace(tx);
        self.clear_last_error().await;
        true
    }

    /// Disconnect and clear the connection state.
    pub async fn disconnect(&self, reason: Option<ElectrumConnectionErr>) {
        self.tx.lock().await.take();
        self.responses.lock().await.clear();
        self.clear_protocol_version().await;
        if let Some(reason) = reason {
            self.set_last_error(reason).await;
        }
        self.abortable_system.abort_all_and_reset().ok();
    }

    /// Sends a request to the electrum server and waits for the response.
    ///
    /// ## Important: This should always return [`JsonRpcErrorType::Transport`] error.
    pub async fn electrum_request(
        &self,
        mut req_json: String,
        rpc_id: JsonRpcId,
        timeout: f64,
    ) -> Result<JsonRpcResponseEnum, JsonRpcErrorType> {
        #[cfg(not(target_arch = "wasm"))]
        {
            // Electrum request and responses must end with \n
            // https://electrumx.readthedocs.io/en/latest/protocol-basics.html#message-stream
            req_json.push('\n');
        }

        // Create a oneshot channel to receive the response in.
        let (req_tx, res_rx) = async_oneshot::channel();
        self.responses.lock().await.insert(rpc_id, req_tx);
        let tx = self
            .tx
            .lock()
            .await
            // Clone to not to hold the lock while sending the request.
            .clone()
            .ok_or_else(|| JsonRpcErrorType::Transport("Connection is not established".to_string()))?;

        // Send the request to the electrum server.
        tx.send(req_json.into_bytes())
            .compat()
            .await
            .map_err(|e| JsonRpcErrorType::Transport(e.to_string()))?;

        // Wait for the response to be processed and sent back to us.
        res_rx
            .timeout_secs(timeout)
            .await
            .map_err(|e| JsonRpcErrorType::Transport(e.to_string()))?
            .map_err(|_e| JsonRpcErrorType::Transport("The sender didn't send".to_string()))
    }

    /// Process an incoming JSONRPC response from the electrum server.
    async fn process_electrum_response(
        &self,
        raw_json: Json,
        event_handlers: &Vec<Box<SharableRpcTransportEventHandler>>,
    ) {
        // Inform the event handlers.
        if let Ok(ref data) = json::to_vec(&raw_json) {
            event_handlers.on_incoming_response(data);
        }

        // detect if we got standard JSONRPC response or subscription response as JSONRPC request
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum ElectrumRpcResponseEnum {
            /// The subscription response as JSONRPC request.
            ///
            /// NOTE Because JsonRpcResponse uses default values for each of its field,
            /// this variant has to stay at top in this enumeration to be properly deserialized
            /// from serde.
            SubscriptionNotification(JsonRpcRequest),
            /// The standard JSONRPC single response.
            SingleResponse(JsonRpcResponse),
            /// The batch of standard JSONRPC responses.
            BatchResponses(JsonRpcBatchResponse),
        }

        let response: ElectrumRpcResponseEnum = match json::from_value(raw_json) {
            Ok(res) => res,
            Err(e) => {
                error!("{}", e);
                return;
            },
        };

        let response = match response {
            ElectrumRpcResponseEnum::SingleResponse(single) => JsonRpcResponseEnum::Single(single),
            ElectrumRpcResponseEnum::BatchResponses(batch) => JsonRpcResponseEnum::Batch(batch),
            ElectrumRpcResponseEnum::SubscriptionNotification(req) => {
                // NOTE: Sending a script hash notification is handled in it's own event handler.

                // FIXME: What is this used for? Note that the id is the method name in this case (two similar id will collide),
                // but this isn't a response to any request anyways, this is a notification, and we forwarded using the
                // scripthash_notification_sender above already.
                JsonRpcResponseEnum::Single(JsonRpcResponse {
                    id: req.method.clone(),
                    jsonrpc: "2.0".into(),
                    result: req.params[0].clone(),
                    error: Json::Null,
                })
                //return; // BLOCKCHAIN_HEADERS_SUB_ID wasn't handled, just returned.
                //also, you might want to check the notification type to print an error message if it's not expected
            },
        };

        // the corresponding sender may not exist, receiver may be dropped
        // these situations are not considered as errors so we just silently skip them
        let pending = self.responses.lock().await.remove(&response.rpc_id());
        if let Some(tx) = pending {
            tx.send(response).ok();
        }
    }

    /// Process a bulk response from the electrum server.
    ///
    /// A bulk response is a response that contains multiple JSONRPC responses.
    async fn process_electrum_bulk_response(
        &self,
        bulk_response: &[u8],
        event_handlers: &Vec<Box<SharableRpcTransportEventHandler>>,
    ) {
        // We should split the received response because we can get several responses in bulk.
        let responses = bulk_response.split(|item| *item == b'\n');

        for response in responses {
            // `split` returns empty slice if it ends with separator which is our case.
            if !response.is_empty() {
                let raw_json: Json = match json::from_slice(response) {
                    Ok(json) => json,
                    Err(e) => {
                        error!("{}", e);
                        return;
                    },
                };
                self.process_electrum_response(raw_json, event_handlers).await
            }
        }
    }

    /// Starts the connection loop that keeps an active connection to the electrum server.
    ///
    /// This will first try to connect to the server and use that connection to query its version.
    /// If version checks succeed, the connection will be kept alive, otherwise, it will be dropped.
    /// Returns either the version of the server, or an [`ElectrumConnectionErr`].
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn establish_connection_loop(
        connection: Arc<ElectrumConnection>,
        client: ElectrumClient,
    ) -> Result<f32, ElectrumConnectionErr> {
        // Check why we errored the last time, don't try to reconnect if it was an irrecoverable error.
        if let Some(last_error) = connection.last_error().await {
            if !last_error.is_recoverable() {
                return Err(last_error);
            }
        }

        let address = connection.address().to_string();
        let event_handlers = client.event_handlers();
        // This is the timeout for connection establishment and version querying (i.e. the whole method).
        // The caller is guaranteed that the method will return within this time.
        let timeout = connection
            .settings
            .timeout_sec
            .unwrap_or(DEFAULT_CONNECTION_ESTABLISHMENT_TIMEOUT);

        let socket_addr = match address.to_socket_addrs() {
            Ok(mut addr) => match addr.next() {
                Some(addr) => addr,
                None => {
                    disconnect_and_return!(Irrecoverable, "Address resolved to None.", connection, event_handlers);
                },
            },
            Err(e) => {
                disconnect_and_return!(
                    Irrecoverable,
                    format!("Resolve error in address: {e:?}"),
                    connection,
                    event_handlers
                );
            },
        };

        let mut secure_connection = false;
        let connect_f = match connection.settings.protocol {
            ElectrumProtocol::TCP => Either::Left(TcpStream::connect(&socket_addr).map_ok(ElectrumStream::Tcp)),
            ElectrumProtocol::SSL => {
                let uri: Uri = match address.parse() {
                    Ok(uri) => uri,
                    Err(e) => {
                        disconnect_and_return!(
                            Irrecoverable,
                            format!("URL parse error: {e:?}"),
                            connection,
                            event_handlers
                        );
                    },
                };

                let Some(dns_name) = uri.host().map(String::from) else {
                    disconnect_and_return!(Irrecoverable, "Couldn't retrieve host from addr",  connection, event_handlers);
                };

                // Make sure that the host is a valid DNS name.
                if let Err(e) = DnsNameRef::try_from_ascii_str(dns_name.as_str()) {
                    disconnect_and_return!(
                        Irrecoverable,
                        format!("Invalid DNS name: {e:?}"),
                        connection,
                        event_handlers
                    )
                }

                let tls_connector = if connection.settings.disable_cert_verification {
                    TlsConnector::from(UNSAFE_TLS_CONFIG.clone())
                } else {
                    secure_connection = true;
                    TlsConnector::from(SAFE_TLS_CONFIG.clone())
                };

                Either::Right(TcpStream::connect(&socket_addr).and_then(move |stream| {
                    // Can use `unwrap` cause `dns_name` is pre-checked.
                    let dns = ServerName::try_from(dns_name.as_str())
                        .map_err(|e| format!("{:?}", e))
                        .unwrap();
                    tls_connector.connect(dns, stream).map_ok(ElectrumStream::Tls)
                }))
            },
            ElectrumProtocol::WS | ElectrumProtocol::WSS => {
                disconnect_and_return!(
                    Irrecoverable,
                    "Incorrect protocol for native connection ('WS'/'WSS'). Use 'TCP' or 'SSL' instead.",
                    connection,
                    event_handlers
                );
            },
        };

        // Try to connect to the server.
        let now = Instant::now();
        let Ok(connect_f) = connect_f.boxed().timeout_secs(timeout).await else {
            disconnect_and_return!(
                ElectrumConnectionErr::Timeout(timeout),
                connection,
                event_handlers
            );
        };
        // We will use the remaining timeout for version checking.
        let timeout = (timeout - now.elapsed().as_secs_f64()).max(0.0);

        let stream = disconnect_and_return_if_err!(connect_f, Temporary, connection, event_handlers);
        disconnect_and_return_if_err!(stream.as_ref().set_nodelay(true), Temporary, connection, event_handlers);

        match secure_connection {
            true => info!("Electrum client connected to {address} securely"),
            false => info!("Electrum client connected to {address}"),
        };

        let (connection_ready_signal, wait_for_connection_ready) = async_oneshot::channel();
        let connection_loop = {
            // Branch 1: Disconnect after not receiving responses for too long.
            let last_response = Arc::new(AtomicU64::new(now_ms()));
            let no_connection_timeout_f = {
                let last_response = last_response.clone();
                async move {
                    loop {
                        Timer::sleep(CUTOFF_TIMEOUT).await;
                        let last_sec = (last_response.load(AtomicOrdering::Relaxed) / 1000) as f64;
                        if now_float() - last_sec > CUTOFF_TIMEOUT {
                            break ElectrumConnectionErr::Temporary(format!(
                                "Server didn't respond for too long ({}s).",
                                now_float() - last_sec
                            ));
                        }
                    }
                }
            };
            let mut no_connection_timeout_f = Box::pin(no_connection_timeout_f).fuse();

            // Branch 2: Read incoming responses from the server.
            let (read, mut write) = tokio::io::split(stream);
            let recv_f = {
                let connection = connection.clone();
                let event_handlers = event_handlers.clone();
                async move {
                    let mut buffer = String::with_capacity(1024);
                    let mut buf_reader = BufReader::new(read);
                    loop {
                        match buf_reader.read_line(&mut buffer).await {
                            Ok(c) => {
                                // FIXME: Why do we close the connection on EOF? Is that a signal from the server
                                // that no more data will be sent over this connection?
                                if c == 0 {
                                    break ElectrumConnectionErr::Temporary("EOF".to_string());
                                }
                            },
                            // FIXME: Fine grain the possible errors here, some of them might be irrecoverable?
                            Err(e) => {
                                break ElectrumConnectionErr::Temporary(format!("Error on read {e:?}"));
                            },
                        };

                        last_response.store(now_ms(), AtomicOrdering::Relaxed);
                        connection
                            .process_electrum_bulk_response(buffer.as_bytes(), &event_handlers)
                            .await;
                        buffer.clear();
                    }
                }
            };
            let mut recv_f = Box::pin(recv_f).fuse();

            // Branch 3: Send outgoing requests to the server.
            let (tx, rx) = mpsc::channel(0);
            let send_f = {
                let address = address.clone();
                let event_handlers = event_handlers.clone();
                let mut rx = rx_to_stream(rx).compat();
                async move {
                    while let Some(Ok(bytes)) = rx.next().await {
                        if let Err(e) = write.write_all(&bytes).await {
                            error!("Write error {e} to {address}");
                        } else {
                            event_handlers.on_outgoing_request(&bytes);
                        }
                    }
                    ElectrumConnectionErr::Temporary("Sender disconnected".to_string())
                }
            };
            let mut send_f = Box::pin(send_f).fuse();

            let address = address.clone();
            let connection = connection.clone();
            let event_handlers = event_handlers.clone();
            async move {
                let already_connected = !connection.try_connect(tx).await;
                // Signal that the connection is up and ready.
                connection_ready_signal.send(()).ok();
                if already_connected {
                    // Gracefully return if some other thread is already connected using `connection`.
                    // Up until this point, we didn't alter `connection` in any way.
                    // Note that the other active connection is the one which will reply to the version querying
                    // and we will still exit `establish_connection_loop` without any errors.
                    return;
                }

                event_handlers.on_connected(&address).ok();
                info!("{address} is now connected");

                let err = select! {
                    e = no_connection_timeout_f => e,
                    e = recv_f => e,
                    e = send_f => e,
                };

                error!("{address} connection dropped due to: {err:?}");
                event_handlers.on_disconnected(&address).ok();
                // FIXME: This will call abort all spawned tasks. INCLUDING THIS ONE WE ARE RUNNING OFF OF.
                connection.disconnect(Some(err)).await;
            }
        };
        // Start the connection loop on a weak spawner.
        connection.weak_spawner().spawn(connection_loop);

        // Wait for the connection to be ready before querying the version.
        let now = Instant::now();
        if wait_for_connection_ready.timeout_secs(timeout).await.is_err() {
            disconnect_and_return!(
                Temporary,
                format!("Timed out ({timeout}s) while waiting for the connection to be ready"),
                connection,
                event_handlers
            );
        }
        // This is the remaining timeout for version checking.
        let timeout = (timeout - now.elapsed().as_secs_f64()).max(0.0);

        let version = match client
            .server_version(&address, client.protocol_version())
            .compat()
            .timeout_secs(timeout)
            .await
        {
            Err(_) => disconnect_and_return!(
                Temporary,
                format!("Timed out ({timeout}s) while querying electrum server version"),
                connection,
                event_handlers
            ),
            Ok(response) => match response {
                Err(e) => disconnect_and_return!(
                    Temporary,
                    format!("Error querying electrum server version {e:?}"),
                    connection,
                    event_handlers
                ),
                Ok(version_str) => match version_str.protocol_version.parse::<f32>() {
                    Err(e) => disconnect_and_return!(
                        // FIXME: Relax the error type here?
                        Irrecoverable,
                        format!("Failed to parse electrum server version {e:?}"),
                        connection,
                        event_handlers
                    ),
                    Ok(version_f32) => {
                        if client.negotiate_version() {
                            let client_version_range = client.protocol_version();
                            if !client_version_range.contains(&version_f32) {
                                disconnect_and_return!(
                                    ElectrumConnectionErr::VersionMismatch(client_version_range.clone(), version_f32),
                                    connection,
                                    event_handlers
                                )
                            }
                        }
                        version_f32
                    },
                },
            },
        };

        connection.set_protocol_version(version).await;
        Ok(version)
    }

    #[cfg(target_arch = "wasm32")]
    pub async fn establish_connection_loop(
        connection: Arc<ElectrumConnection>,
        client: ElectrumClient,
    ) -> Result<f32, ElectrumConnectionErr> {
        use mm2_net::wasm::wasm_ws::ws_transport;
        use std::sync::atomic::AtomicUsize;

        use super::event_handlers;
        lazy_static! {
            static ref CONN_IDX: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        }

        // Check why we errored the last time, don't try to reconnect if it was an irrecoverable error.
        if let Some(last_error) = connection.last_error().await {
            if !last_error.is_recoverable() {
                return Err(last_error);
            }
        }

        let mut address = connection.address().to_string();
        let event_handlers = client.event_handlers();
        // This is the timeout for connection establishment and version querying (i.e. the whole method).
        // The caller is guaranteed that the method will return within this time.
        let timeout = connection
            .settings
            .timeout_sec
            .unwrap_or(DEFAULT_CONNECTION_ESTABLISHMENT_TIMEOUT);

        let uri: Uri = match address.parse() {
            Ok(uri) => uri,
            Err(e) => {
                disconnect_and_return!(
                    Irrecoverable,
                    format!("Failed to parse the address: {e:?}"),
                    connection,
                    event_handlers
                );
            },
        };
        if uri.scheme().is_some() {
            disconnect_and_return!(
                Irrecoverable,
                "There has not to be a scheme in the url. 'ws://' scheme is used by default.  Consider using 'protocol: \"WSS\"' in the electrum request to switch to the 'wss://' scheme.",
                connection,
                event_handlers
            );
        }

        let mut secure_connection = false;
        match connection.settings.protocol {
            ElectrumProtocol::WS => {
                address.insert_str(0, "ws://");
            },
            ElectrumProtocol::WSS => {
                address.insert_str(0, "wss://");
                secure_connection = true;
            },
            ElectrumProtocol::TCP | ElectrumProtocol::SSL => {
                disconnect_and_return!(
                    Irrecoverable,
                    "'TCP' and 'SSL' are not supported in a browser. Please use 'WS' or 'WSS' protocols",
                    connection,
                    event_handlers
                );
            },
        };

        // Try to connect to the server.
        let now = Instant::now();
        let Ok(connect_f) = ws_transport(
            CONN_IDX.fetch_add(1, AtomicOrdering::Relaxed),
            &address,
            &connection.weak_spawner()
        )
        .boxed()
        .timeout_secs(timeout)
        .await else {
            disconnect_and_return!(
                ElectrumConnectionErr::Timeout(timeout),
                address,
                connection
            );
        };
        // We will use the remaining timeout for version checking.
        let timeout = (timeout - now.elapsed().as_secs_f64()).max(0.0);

        let (mut transport_tx, mut transport_rx) =
            disconnect_and_return_if_err!(connect_f, Temporary, address, connection);

        match secure_connection {
            true => info!("Electrum client connected to {address} securely"),
            false => info!("Electrum client connected to {address}"),
        };

        let (connection_ready_signal, wait_for_connection_ready) = async_oneshot::channel();

        let connection_loop = {
            let last_response = Arc::new(AtomicU64::new(now_ms()));
            let no_connection_timeout_f = {
                let last_response = last_response.clone();
                async move {
                    loop {
                        Timer::sleep(CUTOFF_TIMEOUT).await;
                        let last_sec = (last_response.load(AtomicOrdering::Relaxed) / 1000) as f64;
                        if now_float() - last_sec > CUTOFF_TIMEOUT {
                            break ElectrumConnectionErr::Temporary(format!(
                                "Server didn't respond for too long ({}s).",
                                now_float() - last_sec
                            ));
                        }
                    }
                }
            };
            let mut no_connection_timeout_f = Box::pin(no_connection_timeout_f).fuse();

            let recv_f = {
                let address = address.clone();
                let connection = connection.clone();
                let event_handlers = event_handlers.clone();
                async move {
                    // FIXME: `process_electrum_response` would be better off receiving bytes and not raw json,
                    // this is to avoid converting the json back to bytes to pass to the event handlers. Check if
                    // this is possible.
                    while let Some(response) = transport_rx.next().await {
                        match response {
                            Ok(raw_json) => {
                                last_response.store(now_ms(), AtomicOrdering::Relaxed);
                                connection.process_electrum_response(raw_json, &event_handlers).await;
                            },
                            Err(e) => {
                                error!("{address} error: {e:?}");
                            },
                        }
                    }
                    ElectrumConnectionErr::Temporary("Receiver disconnected".to_string())
                }
            };
            let mut recv_f = Box::pin(recv_f).fuse();

            let (tx, rx) = mpsc::channel(0);
            let send_f = {
                let address = address.clone();
                let event_handlers = event_handlers.clone();
                let mut rx = rx_to_stream(rx).compat();
                async move {
                    while let Some(Ok(bytes)) = rx.next().await {
                        // FIXME: Also here we don't need to convert the bytes to json.
                        // That's too early considering it will be send over the network.
                        // Fix `transport_tx.send` so that it accepts bytes instead.
                        let raw_json: Json = match json::from_slice(&bytes) {
                            Ok(raw_json) => raw_json,
                            Err(e) => {
                                error!("{address} error: {e:?} deserializing the outgoing data: {bytes:?}");
                                continue;
                            },
                        };
                        event_handlers.on_outgoing_request(&bytes);

                        if let Err(e) = transport_tx.send(raw_json).await {
                            error!("{address} error: {e:?} sending data {bytes:?}");
                        }
                    }
                    ElectrumConnectionErr::Temporary("Sender disconnected".to_string())
                }
            };
            let mut send_f = Box::pin(send_f).fuse();

            let address = address.clone();
            let connection = connection.clone();
            let event_handlers = event_handlers.clone();
            async move {
                let already_connected = !connection.try_connect(tx).await;
                // Signal that the connection is up and ready.
                connection_ready_signal.send(()).ok();
                if already_connected {
                    // Gracefully return if some other thread is already connected using `connection`.
                    // Up until this point, we didn't alter `connection` in any way.
                    // Note that the other active connection is the one which will reply to the version querying
                    // and we will still exit `establish_connection_loop` without any errors.
                    return;
                }
                event_handlers.on_connected(&address).ok();
                info!("{address} is now connected");

                let err = select! {
                    e = no_connection_timeout_f => e,
                    e = recv_f => e,
                    e = send_f => e,
                };

                error!("{address} connection dropped due to: {err:?}");
                event_handlers.on_disconnected(&address).ok();
                // FIXME: This will call abort all spawned tasks. INCLUDING THIS ONE WE ARE RUNNING OFF OF.
                connection.disconnect(Some(err)).await;
            }
        };
        // Start the connection loop on a weak spawner.
        connection.weak_spawner().spawn(connection_loop);

        // Wait for the connection to be ready before querying the version.
        let now = Instant::now();
        if wait_for_connection_ready.timeout_secs(timeout).await.is_err() {
            disconnect_and_return!(
                Temporary,
                format!("Timed out ({timeout}s) while waiting for the connection to be ready"),
                connection,
                event_handlers
            );
        }
        // This is the remaining timeout for version checking.
        let timeout = (timeout - now.elapsed().as_secs_f64()).max(0.0);

        let version = match client
            .server_version(&address, client.protocol_version())
            .compat()
            .timeout_secs(timeout)
            .await
        {
            Err(_) => disconnect_and_return_error!(
                Temporary,
                format!("Timed out ({timeout}s) while querying electrum server version"),
                connection,
                event_handlers
            ),
            Ok(response) => match response {
                Err(e) => disconnect_and_return_error!(
                    Temporary,
                    format!("Error querying electrum server version {e:?}"),
                    connection,
                    event_handlers
                ),
                Ok(version_str) => match version_str.protocol_version.parse::<f32>() {
                    Err(e) => disconnect_and_return_error!(
                        // FIXME: Relax the error type here?
                        Irrecoverable,
                        format!("Failed to parse electrum server version {e:?}"),
                        connection,
                        event_handlers
                    ),
                    Ok(version_f32) => {
                        if client.negotiate_version() {
                            let client_version_range = client.protocol_version();
                            if !client_version_range.contains(&version_f32) {
                                disconnect_and_return_error!(
                                    ElectrumConnectionErr::VersionMismatch(client_version_range.clone(), version_f32),
                                    connection,
                                    event_handlers
                                )
                            }
                        }
                        version_f32
                    },
                },
            },
        };

        connection.set_protocol_version(version).await;
        Ok(version)
    }
}
