const PING_TIMEOUT_SEC: f64 = 30;
const ELECTRUM_TIMEOUT_SEC: u64 = 60;
const BLOCKCHAIN_HEADERS_SUB_ID: &str = "blockchain.headers.subscribe";
const BLOCKCHAIN_SCRIPTHASH_SUB_ID: &str = "blockchain.scripthash.subscribe";

/// Electrum protocol version verifier.
/// Once a connection is established, it make's sure it is of the correct version.
///
/// FIXME: check the possibility of getting rid of this checked directly after connection
/// making the caller wait until the version is checked to report back to them if this connection will work or not.
/// this requires running the connection loop at first (concurrently) to be able to query the version and then decide
/// whether we want to shut down the connection loop or not.
struct ElectrumProtoVerifier {
    // maybe weak instance of ElectrumClient
}

impl RpcTransportEventHandler for ElectrumProtoVerifier {
    fn debug_info(&self) -> String { "ElectrumProtoVerifier".into() }

    fn on_outgoing_request(&self, _data_len: usize) {}

    fn on_incoming_response(&self, _data_len: usize) {}

    fn on_connected(&self, address: &str) -> Result<(), String> {
        debug!("Connected to the electrum server: {}", address);
        // check version using ElectrumClient
        // best done in another thread to not block the caller.
        Ok(())
    }

    fn on_disconnected(&self, address: &str) -> Result<(), String> {
        debug!("Disconnected from the electrum server: {}", address);
        // reset protocol version
        Ok(())
    }
}

/// An `RpcTransportEventHandler` that forwards `ScripthashNotification`s to trigger balance updates.
///
/// This handler hooks in `on_incoming_response` and looks for an electrum script hash notification to forward it.
struct ElectrumScriptHashNotificationBridge {
    scripthahs_notification_sender: UnboundedSender<ScripthashNotification>,
}

impl RpcTransportEventHandler for ElectrumProtoVerifier {
    fn debug_info(&self) -> String { "ElectrumScriptHashNotificationBridge".into() }

    fn on_outgoing_request(&self, data: &[u8]) {}

    fn on_incoming_response(&self, data: &[u8]) {
        if let Some(raw_json) = json::from_slice::<Json>(data) {
            // Try to parse the notification. A notification is sent as a JSON-RPC request.
            if let Some(notification) = json::from_value::<JsonRpcRequest>(raw_json) {
                // Only care about `BLOCKCHAIN_SCRIPTHASH_SUB_ID` notifications.
                if notification.method.as_ref() == BLOCKCHAIN_SCRIPTHASH_SUB_ID {
                    if let Some(scripthash) = notification.params.first().map(|s| s.as_str()).flatten() {
                        if let Err(e) = self
                            .scripthash_notification_sender
                            .send(ScripthashNotification::Trigger(scripthash.to_string()))
                        {
                            error!("Failed sending script hash message. {e}");
                        }
                    } else {
                        warn!("Notification must contain the script hash value, got: {notification}");
                    }
                };
            }
        }
    }

    fn on_connected(&self, address: &str) -> Result<(), String> {}

    fn on_disconnected(&self, address: &str) -> Result<(), String> {}
}

#[inline]
pub fn electrum_script_hash(script: &[u8]) -> Vec<u8> {
    let mut sha = Sha256::new();
    sha.update(input);
    sha.finalize().to_vec().into_iter().rev().collect()
}

async fn check_electrum_server_version(client: &ElectrumClient, electrum_addr: &str) -> bool {
    async fn remove_server(client: &ElectrumClient, electrum_addr: &str) {
        if let Err(e) = client.remove_server(electrum_addr).await {
            error!("Error on remove server: {}", e);
        }
    }

    let client_name = client.client_name();
    let protocol_version = client.protocol_version();
    debug!("Check version, supported protocols: {:?}", protocol_version);
    let version = match client
        .server_version(electrum_addr, client_name, protocol_version)
        .compat()
        .await
    {
        Ok(version) => version,
        Err(e) => {
            error!("Electrum {} server.version error: {:?}", electrum_addr, e);
            if !e.error.is_transport() {
                remove_server(client, electrum_addr).await;
            };
            return false;
        },
    };

    let actual_version = match version.protocol_version.parse::<f32>() {
        Ok(v) => v,
        Err(e) => {
            error!("Error while parsing protocol_version: {:?}", e);
            remove_server(client, electrum_addr).await;
            return false;
        },
    };

    if !protocol_version.contains(&actual_version) {
        error!(
            "Received unsupported protocol version {:?} from {:?}. Remove the connection",
            actual_version, electrum_addr
        );
        remove_server(client, electrum_addr).await;
        return false;
    }

    match client.set_protocol_version(electrum_addr, actual_version).await {
        Ok(()) => info!(
            "Using protocol version {:?} for Electrum {:?}",
            actual_version, electrum_addr
        ),
        Err(e) => {
            error!("Error on set protocol_version: {}", e);
            return false;
        },
    };

    true
}

/// Helper function casting mpsc::Receiver as Stream.
fn rx_to_stream(rx: mpsc::Receiver<Vec<u8>>) -> impl Stream<Item = Vec<u8>, Error = io::Error> {
    rx.map_err(|_| panic!("errors not possible on rx"))
}

macro_rules! handle_connect_err {
    ($e:expr, $addr: ident) => {
        match $e {
            Ok(res) => res,
            Err(e) => {
                error!("{}", ERRL!("{:?} error {:?}", $addr, e));
                return;
            },
        }
    };
}

/// Starts the connection loop that keeps an active connection to the electrum server.
///
/// This will first try to connect to the server and use that connection to query its version.
/// If version checks succeed, the connection will be kept alive, otherwise, it will be dropped.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
async fn establish_connection_loop<Spawner: SpawnFuture>(
    config: ElectrumConfig,
    addr: String,
    responses: JsonRpcPendingRequestsShared,
    connection_tx: Arc<AsyncMutex<Option<mpsc::Sender<Vec<u8>>>>>,
    event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
    conn_ready_notifier: async_oneshot::Sender<()>,
    scripthash_notification_sender: ScripthashNotificationSender,
    _spawner: Spawner,
) {
    fn addr_to_socket_addr(address: &str) -> Result<SocketAddr, String> {
        match address.to_socket_addrs() {
            Ok(addr) => match addr.next() {
                Some(addr) => Ok(addr),
                None => ERR!("{} resolved to None.", address),
            },
            Err(e) => ERR!("{} resolve error {:?}", address, e),
        }
    }

    let delay = Arc::new(AtomicU64::new(0));

    let current_delay = delay.load(AtomicOrdering::Relaxed);
    if current_delay > 0 {
        Timer::sleep(current_delay as f64).await;
    };

    let socket_addr = handle_connect_err!(addr_to_socket_addr(&addr), addr);

    let mut secure_connection = false;
    let connect_f = match config.clone() {
        ElectrumConfig::TCP => Either::Left(TcpStream::connect(&socket_addr).map_ok(ElectrumStream::Tcp)),
        ElectrumConfig::SSL {
            dns_name,
            skip_validation,
        } => {
            let tls_connector = if skip_validation {
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
    };

    let stream = handle_connect_err!(connect_f.await, addr);
    handle_connect_err!(stream.as_ref().set_nodelay(true), addr);

    match secure_connection {
        true => info!("Electrum client connected to {} securely", addr),
        false => info!("Electrum client connected to {}", addr),
    };

    let (tx, rx) = mpsc::channel(0);

    // We are now officially connected.
    *connection_tx.lock().await = Some(tx);
    handle_connect_err!(
        event_sender.unbounded_send(ElectrumClientEvent::Connected {
            address: addr.clone(),
            conn_ready_notifier,
        }),
        addr
    );

    // Inspect the stream and send events on outgoing requests.
    let rx = rx_to_stream(rx).inspect(|data| {
        // measure the length of each sent packet
        handle_connect_err!(
            event_sender.unbounded_send(ElectrumClientEvent::OutgoingRequest { data_len: data.len() }),
            addr
        );
    });

    let last_chunk = Arc::new(AtomicU64::new(now_ms()));
    // FIXME: Needs proper time-outing (not default) also what is it used for, doesn't pinging do the job??!
    let no_connection_timeout = {
        let last_chunk = last_chunk.clone();
        async move {
            loop {
                Timer::sleep(ELECTRUM_TIMEOUT_SEC as f64).await;
                let last_sec = (last_chunk.load(AtomicOrdering::Relaxed) / 1000) as f64;
                if now_float() - last_sec > ELECTRUM_TIMEOUT_SEC as f64 {
                    warn!(
                        "Didn't receive any data since {}. Shutting down the connection.",
                        last_sec as i64
                    );
                    break;
                }
            }
        }
    };
    let mut no_connection_timeout = Box::pin(no_connection_timeout).fuse();

    let (read, mut write) = tokio::io::split(stream);
    let recv_f = {
        let delay = delay.clone();
        let addr = addr.clone();
        let responses = responses.clone();
        let scripthash_notification_sender = scripthash_notification_sender.clone();
        let event_sender = event_sender.clone();
        async move {
            let mut buffer = String::with_capacity(1024);
            let mut buf_reader = BufReader::new(read);
            loop {
                match buf_reader.read_line(&mut buffer).await {
                    Ok(c) => {
                        if c == 0 {
                            info!("EOF from {}", addr);
                            break;
                        }
                        // reset the delay if we've connected successfully and only if we received some data from connection
                        delay.store(0, AtomicOrdering::Relaxed);
                    },
                    Err(e) => {
                        error!("Error on read {} from {}", e, addr);
                        break;
                    },
                };
                // measure the length of each incoming packet
                handle_connect_err!(
                    event_sender.unbounded_send(ElectrumClientEvent::IncomingResponse { data_len: buffer.len() }),
                    addr
                );
                last_chunk.store(now_ms(), AtomicOrdering::Relaxed);

                electrum_process_chunk(buffer.as_bytes(), &responses, scripthash_notification_sender.clone()).await;
                buffer.clear();
            }
        }
    };
    let mut recv_f = Box::pin(recv_f).fuse();

    let send_f = {
        let addr = addr.clone();
        let mut rx = rx.compat();
        async move {
            while let Some(Ok(bytes)) = rx.next().await {
                if let Err(e) = write.write_all(&bytes).await {
                    error!("Write error {} to {}", e, addr);
                }
            }
        }
    };
    let mut send_f = Box::pin(send_f).fuse();

    select! {
        _ = no_connection_timeout => (),
        _ = recv_f => (),
        _ = send_f => (),
    }

    info!("{} connection dropped", addr);
    // We are now officially disconnected.
    *connection_tx.lock().await = None;
    handle_connect_err!(
        event_sender.unbounded_send(ElectrumClientEvent::Disconnected { address: addr.clone() }),
        addr
    );
}

#[cfg(target_arch = "wasm32")]
#[allow(clippy::too_many_arguments)]
async fn establish_connection_loop<Spawner: SpawnFuture>(
    _config: ElectrumConfig,
    addr: String,
    responses: JsonRpcPendingRequestsShared,
    connection_tx: Arc<AsyncMutex<Option<mpsc::Sender<Vec<u8>>>>>,
    event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
    conn_ready_notifier: async_oneshot::Sender<()>,
    scripthash_notification_sender: ScripthashNotificationSender,
    spawner: Spawner,
) {
    use std::sync::atomic::AtomicUsize;

    lazy_static! {
        static ref CONN_IDX: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    }

    use mm2_net::wasm::wasm_ws::ws_transport;

    let conn_idx = CONN_IDX.fetch_add(1, AtomicOrdering::Relaxed);
    let (mut transport_tx, mut transport_rx) = handle_connect_err!(ws_transport(conn_idx, &addr, &spawner).await, addr);

    info!("Electrum client connected to {}", addr);
    handle_connect_err!(
        event_sender.unbounded_send(ElectrumClientEvent::Connected {
            address: addr.clone(),
            conn_ready_notifier,
        }),
        addr
    );

    let last_chunk = Arc::new(AtomicU64::new(now_ms()));
    let mut last_chunk_fut = electrum_last_chunk_loop(last_chunk.clone()).boxed().fuse();

    let (outgoing_tx, outgoing_rx) = mpsc::channel(0);
    *connection_tx.lock().await = Some(outgoing_tx);

    let incoming_fut = {
        let addr = addr.clone();
        let responses = responses.clone();
        let scripthash_notification_sender = scripthash_notification_sender.clone();
        let event_sender = event_sender.clone();
        async move {
            while let Some(incoming_res) = transport_rx.next().await {
                last_chunk.store(now_ms(), AtomicOrdering::Relaxed);
                match incoming_res {
                    Ok(incoming_json) => {
                        // measure the length of each incoming packet
                        let incoming_str = incoming_json.to_string();
                        handle_connect_err!(
                            event_sender.unbounded_send(ElectrumClientEvent::IncomingResponse {
                                data_len: incoming_str.len()
                            }),
                            addr
                        );
                        electrum_process_json(incoming_json, &responses, &scripthash_notification_sender).await;
                    },
                    Err(e) => {
                        error!("{} error: {:?}", addr, e);
                    },
                }
            }
        }
    };
    let mut incoming_fut = Box::pin(incoming_fut).fuse();

    let outgoing_fut = {
        let addr = addr.clone();
        let mut outgoing_rx = rx_to_stream(outgoing_rx).compat();
        let event_sender = event_sender.clone();
        async move {
            while let Some(Ok(data)) = outgoing_rx.next().await {
                let raw_json: Json = match json::from_slice(&data) {
                    Ok(js) => js,
                    Err(e) => {
                        error!("Error {} deserializing the outgoing data: {:?}", e, data);
                        continue;
                    },
                };
                // measure the length of each sent packet
                handle_connect_err!(
                    event_sender.unbounded_send(ElectrumClientEvent::OutgoingRequest { data_len: data.len() }),
                    addr
                );

                if let Err(e) = transport_tx.send(raw_json).await {
                    error!("Error sending to {}: {:?}", addr, e);
                }
            }
        }
    };
    let mut outgoing_fut = Box::pin(outgoing_fut).fuse();

    macro_rules! reset_tx_and_continue {
        () => {
            info!("{} connection dropped", addr);
            *connection_tx.lock().await = None;
            handle_connect_err!(
                event_sender.unbounded_send(ElectrumClientEvent::Disconnected {
                    address: addr.clone(),
                }),
                addr
            );
        };
    }

    select! {
        _last_chunk = last_chunk_fut => { reset_tx_and_continue!(); },
        _incoming = incoming_fut => { reset_tx_and_continue!(); },
        _outgoing = outgoing_fut => { reset_tx_and_continue!(); },
    }
}

/// Builds up the electrum connection, spawns endless loop that attempts to reconnect to the server
/// in case of connection errors.
/// The function takes `abortable_system` that will be used to spawn Electrum's related futures.
fn electrum_connect(
    addr: String,
    config: ElectrumConfig,
    spawner: WeakSpawner,
    event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
    scripthash_notification_sender: &ScripthashNotificationSender,
) -> (ElectrumConnection, async_oneshot::Receiver<()>) {
    let responses = Arc::new(AsyncMutex::new(JsonRpcPendingRequests::default()));
    let tx = Arc::new(AsyncMutex::new(None));

    let (conn_ready_sender, conn_ready_receiver) = async_oneshot::channel::<()>();
    let fut = establish_connection_loop(
        config.clone(),
        addr.clone(),
        responses.clone(),
        tx.clone(),
        event_sender,
        conn_ready_sender,
        scripthash_notification_sender.clone(),
        spawner.clone(),
    );

    spawner.spawn(fut);
    (
        ElectrumConnection {
            addr,
            tx,
            responses,
            protocol_version: AsyncMutex::new(None),
        },
        conn_ready_receiver,
    )
}

/// Attempts to process the request (parse url, etc), build up the config and create new electrum connection
/// The function takes `abortable_system` that will be used to spawn Electrum's related futures.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_electrum(
    conn_settings: &ElectrumConnectionSettings,
    spawner: WeakSpawner,
    event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
    scripthash_notification_sender: &ScripthashNotificationSender,
) -> Result<(ElectrumConnection, async_oneshot::Receiver<()>), ConnectionManagerErr> {
    let config = match conn_settings.protocol {
        ElectrumProtocol::TCP => ElectrumConfig::TCP,
        ElectrumProtocol::SSL => {
            let uri: Uri = conn_settings.url.parse().map_err(|err: InvalidUri| {
                ConnectionManagerErr::ConnectingError(conn_settings.url.to_string(), err.to_string())
            })?;
            let host = uri.host().ok_or_else(|| {
                ConnectionManagerErr::ConnectingError(
                    conn_settings.url.to_string(),
                    "Couldn't retrieve host from addr".to_string(),
                )
            })?;
            DnsNameRef::try_from_ascii_str(host)
                .map_err(|err| ConnectionManagerErr::ConnectingError(conn_settings.url.clone(), err.to_string()))?;

            ElectrumConfig::SSL {
                dns_name: host.into(),
                skip_validation: conn_settings.disable_cert_verification,
            }
        },
        ElectrumProtocol::WS | ElectrumProtocol::WSS => {
            return Err(ConnectionManagerErr::ConnectingError(
                conn_settings.url.clone(),
                "'ws' and 'wss' protocols are not supported yet. Consider using 'TCP' or 'SSL'".to_string(),
            ));
        },
    };

    Ok(electrum_connect(
        conn_settings.url.clone(),
        config,
        spawner,
        event_sender,
        scripthash_notification_sender,
    ))
}

/// Attempts to process the request (parse url, etc), build up the config and create new electrum connection
/// The function takes `abortable_system` that will be used to spawn Electrum's related futures.
#[cfg(target_arch = "wasm32")]
fn spawn_electrum(
    conn_settings: &ElectrumConnectionSettings,
    spawner: WeakSpawner,
    event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
    scripthash_notification_sender: &ScripthashNotificationSender,
) -> Result<(ElectrumConnection, async_oneshot::Receiver<()>), ConnectionManagerErr> {
    let mut url = conn_settings.url.clone();
    let uri: Uri = conn_settings
        .url
        .parse()
        .map_err(|err: InvalidUri| ConnectionManagerErr::ConnectingError(url.clone(), err.to_string()))?;

    if uri.scheme().is_some() {
        return Err(ConnectionManagerErr::ConnectingError(
            url,
            "There has not to be a scheme in the url: {}. \
            'ws://' scheme is used by default. \
            Consider using 'protocol: \"WSS\"' in the electrum request to switch to the 'wss://' scheme."
                .to_string(),
        ));
    }

    let config = match conn_settings.protocol {
        ElectrumProtocol::WS => {
            url.insert_str(0, "ws://");
            ElectrumConfig::WS
        },
        ElectrumProtocol::WSS => {
            url.insert_str(0, "wss://");
            ElectrumConfig::WSS
        },
        ElectrumProtocol::TCP | ElectrumProtocol::SSL => {
            return Err(ConnectionManagerErr::ConnectingError(
                url,
                "'TCP' and 'SSL' are not supported in a browser. Please use 'WS' or 'WSS' protocols".to_string(),
            ));
        },
    };

    Ok(electrum_connect(
        conn_settings.url.clone(),
        config,
        spawner,
        event_sender,
        scripthash_notification_sender,
    ))
}

/// Sends a request to the electrum server and waits for the response.
///
/// ## Important: This should always return [`JsonRpcErrorType::Transport`] error.
fn electrum_request(
    mut req_json: String,
    rpc_id: JsonRpcId,
    tx: mpsc::Sender<Vec<u8>>,
    responses: JsonRpcPendingRequestsShared,
    timeout: u64,
) -> Box<dyn Future<Item = JsonRpcResponseEnum, Error = JsonRpcErrorType> + Send + 'static> {
    let send_fut = async move {
        #[cfg(not(target_arch = "wasm"))]
        {
            // Electrum request and responses must end with \n
            // https://electrumx.readthedocs.io/en/latest/protocol-basics.html#message-stream
            req_json.push('\n');
        }
        // Create a oneshot channel to receive the response in.
        let (req_tx, resp_rx) = async_oneshot::channel();
        responses.lock().await.insert(rpc_id, req_tx);
        tx.send(req_json.into_bytes())
            .compat()
            .await
            .map_err(|err| JsonRpcErrorType::Transport(err.to_string()))?;
        let resps = resp_rx.await.map_err(|e| JsonRpcErrorType::Transport(e.to_string()))?;
        Ok(resps)
    };
    let send_fut = send_fut
        .boxed()
        .timeout(Duration::from_secs(timeout))
        .compat()
        .then(move |res| res.map_err(|err| JsonRpcErrorType::Transport(err.to_string()))?);
    Box::new(send_fut)
}

async fn electrum_request_multi(
    client: ElectrumClient,
    request: JsonRpcRequestEnum,
) -> Result<(JsonRpcRemoteAddr, JsonRpcResponseEnum), JsonRpcErrorType> {
    let mut futures = vec![];
    let connections = client.connection_manager.get_connection().await;

    for (i, connection) in connections.iter().enumerate() {
        let connection = connection.lock().await;
        if client.negotiate_version && connection.protocol_version.lock().await.is_none() {
            continue;
        }

        let connection_addr = connection.addr.clone();
        let json = json::to_string(&request).map_err(|e| JsonRpcErrorType::InvalidRequest(e.to_string()))?;
        // FIXME: You are detecting if connection is established using `.tx` and not that the connection is
        // there in the first place (i.e. not a `None`, and it won't be since the connection manager returned it)
        // Pick one of the two methods and stick with it.
        if let Some(tx) = &*connection.tx.lock().await {
            let fut = electrum_request(
                json,
                request.rpc_id(),
                tx.clone(),
                connection.responses.clone(),
                // FIXME: Why divide by this number? I see this gives more clearance for the last connection, but why?
                ELECTRUM_TIMEOUT_SEC / (connections.len() - i) as u64,
            )
            .map(|response| (JsonRpcRemoteAddr(connection_addr), response));
            futures.push(fut)
        };
    }
    drop(connections);

    if futures.is_empty() {
        return Err(JsonRpcErrorType::Transport(
            "All electrums are currently disconnected".to_string(),
        ));
    }

    if let JsonRpcRequestEnum::Single(single) = &request {
        if single.method == "server.ping" {
            // server.ping must be sent to all servers to keep all connections alive
            return select_ok(futures).map(|(result, _)| result).compat().await;
        }
    }

    let (res, no_of_failed_requests) = select_ok_sequential(futures)
        .compat()
        .await
        .map_err(|e| JsonRpcErrorType::Transport(format!("{:?}", e)))?;
    client.rotate_servers(no_of_failed_requests).await;

    Ok(res)
}

async fn electrum_request_to(
    client: ElectrumClient,
    request: JsonRpcRequestEnum,
    to_addr: String,
) -> Result<(JsonRpcRemoteAddr, JsonRpcResponseEnum), JsonRpcErrorType> {
    let (tx, responses) = {
        let conn = client
            .connection_manager
            .get_connection_by_address(to_addr.as_ref())
            .await;
        let conn = conn.map_err(|err| JsonRpcErrorType::Internal(err.to_string()))?;
        let guard = conn.lock().await;
        let connection: &ElectrumConnection = guard.deref();

        let responses = connection.responses.clone();
        let tx = {
            match &*connection.tx.lock().await {
                Some(tx) => tx.clone(),
                None => {
                    return Err(JsonRpcErrorType::Transport(format!(
                        "Connection {} is not established yet",
                        to_addr
                    )))
                },
            }
        };

        (tx, responses)
    };
    let json = json::to_string(&request).map_err(|err| JsonRpcErrorType::InvalidRequest(err.to_string()))?;
    let response = electrum_request(json, request.rpc_id(), tx, responses, ELECTRUM_TIMEOUT_SEC)
        .compat()
        .await?;
    Ok((JsonRpcRemoteAddr(to_addr.to_owned()), response))
}
