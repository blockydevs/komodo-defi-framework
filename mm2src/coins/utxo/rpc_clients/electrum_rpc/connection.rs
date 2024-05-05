pub type JsonRpcPendingRequests = HashMap<JsonRpcId, async_oneshot::Sender<JsonRpcResponseEnum>>;
use serde::Serialize;
use serde_json::{self as json, Value as Json};

use common::log::{debug, error, info, warn};

cfg_native! {
    use futures::future::Either;
    use futures::io::Error;
    use http::header::AUTHORIZATION;
    use http::{Request, StatusCode};
    use rustls::client::ServerCertVerified;
    use rustls::{Certificate, ClientConfig, ServerName, OwnedTrustAnchor, RootCertStore};
    use std::convert::TryFrom;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::SystemTime;
    use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
    use tokio::net::TcpStream;
    use tokio_rustls::{client::TlsStream, TlsConnector};
    use tokio_rustls::webpki::DnsNameRef;
    use webpki_roots::TLS_SERVER_ROOTS;
}
use super::super::*;
use crate::{big_decimal_from_sat_unsigned, NumConversError, RpcTransportEventHandler, RpcTransportEventHandlerShared,
            SharableRpcTransportEventHandler};

macro_rules! log_and_return {
    ($typ:tt, $err:expr, $addr:expr) => {{
        let err = ElectrumConnectionErr::$typ($err.to_string());
        error!("{} {:?}", $addr, err);
        return Err(err);
    }};
}

macro_rules! log_and_return_if_err {
    ($ex:expr, $typ:tt, $addr:expr) => {{
        match $ex {
            Ok(res) => res,
            Err(e) => {
                log_and_return!($typ, e, $addr);
            },
        }
    }};
}

macro_rules! disconnect_and_return_error {
    ($err:expr, $addr:expr, $connection:expr, $event_handlers:expr) => {{
        error!("{} connection dropped due to: {:?}", $addr, $err);
        // Inform the event handlers of the disconnection.
        $event_handlers.on_disconnected(&$addr);
        // Disconnect the connection.
        $connection.disconnect(Some($err.clone())).await;
        return Err($err);
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
    pub timeout_sec: Option<u64>,
}

#[derive(Clone, Debug)]
enum ElectrumConnectionErr {
    Timeout(f32),
    Temporary(String),
    Irrecoverable(String),
    VersionMismatch(OrdRange<f32>, f32),
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
    async fn new(settings: ElectrumConnectionSettings) -> Self {
        ElectrumConnection {
            settings,
            tx: AsyncMutex::new(None),
            responses: AsyncMutex::new(JsonRpcPendingRequests::new()),
            protocol_version: AsyncMutex::new(None),
            last_error: AsyncMutex::new(None),
            abortable_system: AbortableQueue::default(),
        }
    }

    pub fn address(&self) -> &str { &self.settings.url }

    fn weak_spawner(&self) -> WeakSpawner { self.abortable_system.weak_spawner() }

    async fn set_protocol_version(&self, version: f32) { self.protocol_version.lock().await.replace(version); }

    async fn get_protocol_version(&self) -> Option<f32> { *self.protocol_version.lock().await }

    async fn is_connected(&self) -> bool { self.tx.lock().await.is_some() }

    async fn connect(&self, tx: mpsc::Sender<Vec<u8>>) {
        // Make sure we are disconnected first.
        self.disconnect(None).await;
        // We don't know the server version, the caller should run a connection loop and query the server version to set it.
        self.tx.lock().await.replace(tx);
        self.last_error.lock().await.take();
    }

    /// Disconnect and clear the connection state.
    async fn disconnect(&self, reason: Option<ElectrumConnectionErr>) {
        self.tx.lock().await.take();
        self.responses.lock().await.clear();
        self.protocol_version.lock().await.take();
        *self.last_error.lock().await = reason;
        self.abortable_system.abort_all();
    }

    /// Sends a request to the electrum server and waits for the response.
    ///
    /// ## Important: This should always return [`JsonRpcErrorType::Transport`] error.
    pub async fn electrum_request(
        &self,
        mut req_json: String,
        rpc_id: JsonRpcId,
        timeout: u64,
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
            .timeout(Duration::from_secs(timeout))
            .await
            .map_err(|e| JsonRpcErrorType::Transport(e.to_string()))?
            .map_err(|e| JsonRpcErrorType::Transport("The sender didn't send".to_string()))
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
                //return;
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
    #[cfg(not(target_arch = "wasm32"))]
    async fn establish_connection_loop(
        connection: Arc<ElectrumConnection>,
        client: ElectrumClient,
    ) -> Result<(), ElectrumConnectionErr> {
        let event_handlers = client.event_handlers();
        let address = connection.address().to_string();

        let socket_addr = match address.to_socket_addrs() {
            Ok(mut addr) => match addr.next() {
                Some(addr) => addr,
                None => {
                    log_and_return!(Irrecoverable, "Address resolved to None.", address);
                },
            },
            Err(e) => {
                log_and_return!(Irrecoverable, format!("Resolve error in address: {e:?}"), address);
            },
        };

        let mut secure_connection = false;
        let connect_f = match connection.settings.protocol {
            ElectrumProtocol::TCP => Either::Left(TcpStream::connect(&socket_addr).map_ok(ElectrumStream::Tcp)),
            ElectrumProtocol::SSL => {
                let uri: Uri = match address.parse() {
                    Ok(uri) => uri,
                    Err(e) => {
                        log_and_return!(Irrecoverable, format!("URL parse error: {e:?}"), address);
                    },
                };

                let Some(dns_name) = uri.host().map(String::from) else {
                    log_and_return!(Irrecoverable, "Couldn't retrieve host from addr", address);
                };

                // Make sure that the host is a valid DNS name.
                if let Err(e) = DnsNameRef::try_from_ascii_str(dns_name.as_str()) {
                    log_and_return!(Irrecoverable, format!("Invalid DNS name: {e:?}"), address)
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
                log_and_return!(
                    Irrecoverable,
                    "WS and WSS protocols are not supported yet. Consider using TCP or SSL",
                    address
                );
            },
        };

        let mut timeout = connection.settings.timeout_sec.unwrap_or(ELECTRUM_TIMEOUT_SEC);
        // FIXME: Add a timeout for connection establishment.
        // Use what's left of the timeout to query for the server version.
        let stream = log_and_return_if_err!(connect_f.await, Temporary, address);
        log_and_return_if_err!(stream.as_ref().set_nodelay(true), Temporary, address);

        match secure_connection {
            true => info!("Electrum client connected to {address} securely"),
            false => info!("Electrum client connected to {address}"),
        };

        // We are now connected to the electrum server.
        let (tx, rx) = mpsc::channel(0);
        connection.tx.lock().await.replace(tx);
        event_handlers.on_connected(&address);

        let last_response = Arc::new(AtomicU64::new(now_ms()));
        let no_connection_timeout = {
            let last_response = last_response.clone();
            async move {
                loop {
                    Timer::sleep(ELECTRUM_TIMEOUT_SEC as f64).await;
                    let last_sec = (last_response.load(AtomicOrdering::Relaxed) / 1000) as f64;
                    if now_float() - last_sec > ELECTRUM_TIMEOUT_SEC as f64 {
                        break ElectrumConnectionErr::Temporary(format!(
                            "Server didn't respond for too long ({}s).",
                            now_float() - last_sec
                        ));
                    }
                }
            }
        };
        let mut no_connection_timeout = Box::pin(no_connection_timeout).fuse();

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

        let send_f = {
            let address = address.clone();
            let event_handlers = event_handlers.clone();
            let mut rx = rx_to_stream(rx).compat();
            async move {
                while let Some(Ok(bytes)) = rx.next().await {
                    if let Err(e) = write.write_all(&bytes).await {
                        error!("Write error {e} to {address}");
                    } else {
                        event_handlers.deref().on_outgoing_request(&bytes);
                    }
                }
                ElectrumConnectionErr::Temporary("Sender disconnected".to_string())
            }
        };
        let mut send_f = Box::pin(send_f).fuse();

        // Start the connection loop on a weak spawner.
        let connection_loop = {
            let address = address.clone();
            let connection = connection.clone();
            let event_handlers = event_handlers.clone();
            async move {
                let err = select! {
                    e = no_connection_timeout => e,
                    e = recv_f => e,
                    e = send_f => e,
                };

                error!("{address} connection dropped due to: {err:?}");
                event_handlers.on_disconnected(&address);
                // DISCUSS: This will call abort all spawned tasks. INCLUDING THIS ONE WE ARE RUNNING OFF OF.
                connection.disconnect(Some(err)).await;
            }
        };
        connection.weak_spawner().spawn(connection_loop);

        // FIXME: Use the remainder of the connection timeout here.
        let version = match client
            .server_version(&address, client.protocol_version())
            .compat()
            .await
        {
            Err(e) => disconnect_and_return_error!(
                ElectrumConnectionErr::Temporary(format!("Error querying electrum server version {e:?}")),
                address,
                connection,
                event_handlers
            ),
            Ok(version_str) => match version_str.protocol_version.parse::<f32>() {
                Err(e) => disconnect_and_return_error!(
                    ElectrumConnectionErr::Irrecoverable(format!("Failed to parse electrum server version {e:?}")),
                    address,
                    connection,
                    event_handlers
                ),
                Ok(version_f32) => {
                    if client.negotiate_version() {
                        let client_version_range = client.protocol_version();
                        if !client_version_range.contains(&version_f32) {
                            disconnect_and_return_error!(
                                ElectrumConnectionErr::VersionMismatch(client_version_range.clone(), version_f32),
                                address,
                                connection,
                                event_handlers
                            )
                        }
                    }
                    version_f32
                },
            },
        };

        connection.set_protocol_version(version).await;
        return Ok(());
    }
}
