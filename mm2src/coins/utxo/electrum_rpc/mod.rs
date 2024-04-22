const PING_TIMEOUT_SEC: f64 = 30;
const ELECTRUM_TIMEOUT_SEC: u64 = 60;
const BLOCKCHAIN_HEADERS_SUB_ID: &str = "blockchain.headers.subscribe";
const BLOCKCHAIN_SCRIPTHASH_SUB_ID: &str = "blockchain.scripthash.subscribe";

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
/// FIXME: Why is this needed
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
async fn establish_connection_loop(connection: &ElectrumConnection) -> Result<(), ElectrumConnectionErr> {
    let address = connection.settings.url.clone();
    let socket_addr = match address.to_socket_addrs() {
        Ok(addr) => match addr.next() {
            Some(addr) => Ok(addr),
            None => {
                return Err(ElectrumConnectionErr::Irrecoverable(format!(
                    "Address {address} resolved to None."
                )))
            },
        },
        Err(e) => {
            return Err(ElectrumConnectionErr::Irrecoverable(format!(
                "Resolve error in address {address}: {e:?}."
            )))
        },
    };

    let mut secure_connection = false;
    let connect_f = match connection.settings.protocol {
        ElectrumProtocol::TCP => Either::Left(TcpStream::connect(&socket_addr).map_ok(ElectrumStream::Tcp)),
        ElectrumProtocol::SSL => {
            let uri: Uri = address
                .parse()
                .map_err(|err: InvalidUri| ElectrumConnectionErr::Irrecoverable(err.to_string()))?;
            let dns_name = uri
                .host()
                .ok_or_else(|| ElectrumConnectionErr::Irrecoverable("Couldn't retrieve host from addr".to_string()))?;
            // Make sure that the host is a valid DNS name.
            DnsNameRef::try_from_ascii_str(dns_name)
                .map_err(|err| ElectrumConnectionErr::Irrecoverable(err.to_string()))?;

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
            return Err(ElectrumConnectionErr::Irrecoverable(
                "'ws' and 'wss' protocols are not supported yet. Consider using 'TCP' or 'SSL'".to_string(),
            ));
        },
    };

    let stream = handle_connect_err!(connect_f.await, addr);
    handle_connect_err!(stream.as_ref().set_nodelay(true), addr);

    match secure_connection {
        true => info!("Electrum client connected to {} securely", addr),
        false => info!("Electrum client connected to {}", addr),
    };

    // We are now connected to the electrum server, but let's check the version before calling
    // any `on_connected` events.
    let (tx, rx) = mpsc::channel(0);
    connection.tx.lock().await.replace(tx);
    //event_handlers.on_connected(addr);

    /// FIXME, do another loop for pinging
    let last_response = Arc::new(AtomicU64::new(now_ms()));
    let no_connection_timeout = {
        let last_response = last_response.clone();
        async move {
            loop {
                Timer::sleep(ELECTRUM_TIMEOUT_SEC as f64).await;
                let last_sec = (last_response.load(AtomicOrdering::Relaxed) / 1000) as f64;
                if now_float() - last_sec > ELECTRUM_TIMEOUT_SEC as f64 {
                    warn!(
                        "Didn't receive any data since {}. Shutting down the connection.",
                        last_sec as i64
                    );
                    break ElectrumConnectionErr::Temporary(format!(
                        "Address {address} didn't respond for too long ({}s).",
                        now_float() - last_sec
                    ));
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
                        // FIXME: Why do we close the connection on EOF? Is that a signal from the server
                        // that no more data will be sent over this connection?
                        if c == 0 {
                            info!("EOF from {}", address);
                            break ElectrumConnectionErr::Temporary(format!("EOF from {}", address));
                        }
                        // Reset the delay if we've connected successfully and only if we received some data from connection.
                        delay.store(0, AtomicOrdering::Relaxed);
                    },
                    // FIXME: Fine grain the possible errors here, some of them might be irrecoverable?
                    Err(e) => {
                        error!("Error on read {} from {}", e, addr);
                        break ElectrumConnectionErr::Temporary(format!("Error on read {} from {}", e, address));
                    },
                };

                last_response.store(now_ms(), AtomicOrdering::Relaxed);
                process_electrum_bulk_response(buffer.as_bytes(), event_handlers).await;
                buffer.clear();
            }
        }
    };
    let mut recv_f = Box::pin(recv_f).fuse();

    let send_f = {
        let addr = addr.clone();
        let mut rx = rx.rx_to_stream(rx).compat();
        async move {
            while let Some(Ok(bytes)) = rx.next().await {
                if let Err(e) = write.write_all(&bytes).await {
                    error!("Write error {} to {}", e, addr);
                } else {
                    event_handlers.on_outgoing_request(&bytes);
                }
            }
            ElectrumConnectionErr::Temporary(format!("Sender disconnected for address: {address}"))
        }
    };
    let mut send_f = Box::pin(send_f).fuse();

    let error = select! {
        _ = no_connection_timeout => (),
        _ = recv_f => (),
        _ = send_f => (),
        // FIXME: add another branch for version checking
    };
    // spawn the select above in another thread and validate the version here
    // FIXME: we might want to fire `on_connect` event handlers before version checking because otherwise
    // the handler might get confused (receiving on_outgoing_request & incoming_response events before on_connected event).

    // FIXME: do another select! which will carry out the main connection for now on.
    // replace the version checking branch with a pinger branch.


    info!("{} connection dropped", addr);
    connection.disconnect(Some(error));
    event_handlers.on_disconnected(addr);
}

#[cfg(target_arch = "wasm32")]
#[allow(clippy::too_many_arguments)]
async fn establish_connection_loop<Spawner: SpawnFuture>(
    addr: String,
    responses: JsonRpcPendingRequestsShared,
    connection_tx: Arc<AsyncMutex<Option<mpsc::Sender<Vec<u8>>>>>,
    event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
    conn_ready_notifier: async_oneshot::Sender<()>,
    scripthash_notification_sender: ScripthashNotificationSender,
    spawner: Spawner,
) {
    use mm2_net::wasm::wasm_ws::ws_transport;
    use std::sync::atomic::AtomicUsize;
    lazy_static! {
        static ref CONN_IDX: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    }

    let conn_idx = CONN_IDX.fetch_add(1, AtomicOrdering::Relaxed);
    let (mut transport_tx, mut transport_rx) = handle_connect_err!(ws_transport(conn_idx, &addr, &spawner).await, addr);

    if negotiate_version {
        // FIXME: Fire up a connection (select! with timeout-er branch) and use it to negotiate the version.
        // check_electrum_server_version
    }

    info!("Electrum client connected to {}", addr);
    event_handlers.on_connected(addr).await;

    let last_response = Arc::new(AtomicU64::new(now_ms()));
    let no_connection_timeout = {
        let last_response = last_response.clone();
        async move {
            loop {
                Timer::sleep(ELECTRUM_TIMEOUT_SEC as f64).await;
                let last_sec = (last_response.load(AtomicOrdering::Relaxed) / 1000) as f64;
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

    let (outgoing_tx, outgoing_rx) = mpsc::channel(0);
    *connection_tx.lock().await = Some(outgoing_tx);

    let incoming_fut = {
        let addr = addr.clone();
        let responses = responses.clone();
        let scripthash_notification_sender = scripthash_notification_sender.clone();
        let event_sender = event_sender.clone();
        async move {
            while let Some(incoming_res) = transport_rx.next().await {
                last_response.store(now_ms(), AtomicOrdering::Relaxed);
                match incoming_res {
                    Ok(incoming_json) => {
                        process_electrum_response(incoming_json, &event_handlers).await;
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

    select! {
        _ = no_connection_timeout => (),
        _ = incoming_fut => (),
        _ = outgoing_fut => (),
    }

    info!("{} connection dropped", addr);
    connection.disconnect();
    event_handlers.on_disconnected(addr).await;
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
