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

/// Electrum client configuration
#[allow(clippy::upper_case_acronyms)]
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, Serialize)]
enum ElectrumConfig {
    TCP,
    SSL { dns_name: String, skip_validation: bool },
}

/// Electrum client configuration
#[allow(clippy::upper_case_acronyms)]
#[cfg(target_arch = "wasm32")]
#[derive(Clone, Debug, Serialize)]
enum ElectrumConfig {
    WS,
    WSS,
}

#[derive(Debug)]
pub(super) struct ElectrumClientSettings {
    pub(super) client_name: String,
    pub(super) servers: Vec<ElectrumConnectionSettings>,
    pub(super) coin_ticker: String,
    pub(super) negotiate_version: bool,
    pub(super) connection_manager_policy: ConnectionManagerPolicy,
}

pub enum DisconnectionReason {
    VersionMismatch,
    Timeout,
}

/// Represents the active Electrum connection to selected address
#[derive(Debug)]
pub struct ElectrumConnection {
    /// The client connected to this SocketAddr
    addr: String,
    /// The Sender forwarding requests to writing part of underlying stream
    tx: AsyncMutex<Option<mpsc::Sender<Vec<u8>>>>,
    /// Responses are stored here
    responses: AsyncMutex<JsonRpcPendingRequests>,
    /// Selected protocol version. The value is initialized after the server.version RPC call.
    protocol_version: AsyncMutex<Option<f32>>,
    /// Why was the connection disconnected the last time?
    disconnected_because: AsyncMutex<Option<DisconnectionReason>>,
}

impl ElectrumConnection {
    async fn address(&self) -> &str { &self.addr }

    async fn set_protocol_version(&self, version: f32) { self.protocol_version.lock().await.replace(version); }

    async fn get_protocol_version(&self) -> Option<f32> { self.protocol_version.lock().await }

    async fn is_connected(&self) -> bool { self.tx.lock().await.is_some() }

    async fn connect(&self, tx: mpsc::Sender<Vec<u8>>) {
        // Make sure we are disconnected first.
        self.disconnect().await;
        // We don't know the server version, the caller should run a connection loop and query the server version to set it.
        self.tx.lock().await.replace(tx);
        self.disconnected_because.lock().await.take();
    }

    /// Disconnect and clear the connection state.
    async fn disconnect(&self, reason: DisconnectionReason) {
        self.tx.lock().await.take();
        self.responses.lock().await.clear();
        self.protocol_version.lock().await.take();
        self.disconnected_because.lock().await.replace(reason);
    }

    /// Process an incoming JSONRPC response from the electrum server.
    async fn electrum_process_json(&self, raw_json: Json, event_handlers: &Vec<RpcTransportEventHandlerShared>) {
        event_handlers.on_incoming_response(raw_json.as_bytes());

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
        let mut pending = self.responses.lock().await;
        if let Some(tx) = pending.remove(&response.rpc_id()) {
            tx.send(response).ok();
        }
    }

    /// Process a bulk response from the electrum server.
    ///
    /// A bulk response is a response that contains multiple JSONRPC responses.
    async fn electrum_process_chunk(&self, chunk: &[u8], event_handlers: &Vec<RpcTransportEventHandlerShared>) {
        // we should split the received chunk because we can get several responses in 1 chunk.
        let split = chunk.split(|item| *item == b'\n');

        for chunk in split {
            // split returns empty slice if it ends with separator which is our case
            if !chunk.is_empty() {
                let raw_json: Json = match json::from_slice(chunk) {
                    Ok(json) => json,
                    Err(e) => {
                        error!("{}", e);
                        return;
                    },
                };
                self.electrum_process_json(raw_json, event_handlers).await
            }
        }
    }
}

#[derive(Debug)]
pub struct ElectrumClientImpl {
    client_name: String,
    coin_ticker: String,
    connection_manager: Arc<dyn Deref<Target = dyn ConnectionManagerTrait + Send + Sync> + Send + Sync>,
    next_id: AtomicU64,
    protocol_version: OrdRange<f32>,
    // FIXME: What are these used for? Looks like `ConcurrentRequestMap` is used for caching already running requests
    // to not execute them again. This would make sense if we perform such a request a lot & it's expensive to perform.
    // Also, if `ConcurrentRequestMap` is needed, should we can also consider improving it with a cache timeout mechanism.
    get_balance_concurrent_map: ConcurrentRequestMap<String, ElectrumBalance>,
    list_unspent_concurrent_map: ConcurrentRequestMap<String, Vec<ElectrumUnspent>>,
    block_headers_storage: BlockHeaderStorage,
    negotiate_version: bool,
    /// This is used for balance event streaming implementation for UTXOs.
    /// If balance event streaming isn't enabled, this value will always be `None`; otherwise,
    /// it will be used for sending scripthash messages to trigger re-connections, re-fetching the balances, etc.
    scripthash_notification_sender: ScripthashNotificationSender,
    abortable_system: AbortableQueue,
}

#[cfg_attr(test, mockable)]
impl ElectrumClientImpl {
    pub(super) fn try_new(
        client_settings: ElectrumClientSettings,
        block_headers_storage: BlockHeaderStorage,
        abortable_system: AbortableQueue,
        event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
        scripthash_notification_sender: ScripthashNotificationSender,
    ) -> Result<ElectrumClientImpl, String> {
        let sub_abortable_system = abortable_system
            .create_subsystem()
            .map_err(|err| ERRL!("Failed to create connection_manager abortable system: {}", err))?;
        debug!("Init connection_manager with settings: {:?}", client_settings);
        let mut rng = small_rng();
        let mut servers = client_settings.servers;
        servers.as_mut_slice().shuffle(&mut rng);
        let connection_manager: Arc<dyn Deref<Target = dyn ConnectionManagerTrait + Send + Sync> + Send + Sync> =
            match client_settings.connection_manager_policy {
                ConnectionManagerPolicy::Selective => Arc::new(ConnectionManagerSelective::try_new(
                    servers,
                    sub_abortable_system,
                    event_sender,
                    scripthash_notification_sender.clone(),
                )?),
                ConnectionManagerPolicy::Multiple => Arc::new(ConnectionManagerMultiple::try_new(
                    servers,
                    sub_abortable_system,
                    event_sender,
                    scripthash_notification_sender.clone(),
                )?),
            };
        let protocol_version = OrdRange::new(1.2, 1.4).unwrap();
        Ok(ElectrumClientImpl {
            client_name: client_settings.client_name,
            coin_ticker: client_settings.coin_ticker,
            connection_manager,
            next_id: 0.into(),
            protocol_version,
            get_balance_concurrent_map: ConcurrentRequestMap::new(),
            list_unspent_concurrent_map: ConcurrentRequestMap::new(),
            block_headers_storage,
            negotiate_version: client_settings.negotiate_version,
            abortable_system,
            scripthash_notification_sender,
        })
    }

    // FIXME: Make sure a connection was established here at connect
    pub async fn connect(&self) -> Result<(), String> {
        debug!("electrum_client_impl connect");
        self.connection_manager.connect().await.map_err(|err| err.to_string())
    }

    /// Remove an Electrum connection and stop corresponding spawned actor.
    pub async fn remove_server(&self, server_addr: &str) -> Result<(), String> {
        self.connection_manager
            .remove_server(server_addr)
            .await
            .map_err(|err| err.to_string())
    }

    /// Moves the Electrum servers that fail in a multi request to the end.
    pub async fn rotate_servers(&self, no_of_rotations: usize) {
        self.connection_manager.rotate_servers(no_of_rotations).await
    }

    /// Check if one of the spawned connections is connected.
    pub async fn is_connected(&self) -> bool { self.connection_manager.is_connected().await }

    /// Check if all connections have been removed.
    pub async fn is_connections_pool_empty(&self) -> bool { self.connection_manager.is_connections_pool_empty().await }

    /// Set the protocol version for the specified server.
    pub async fn set_protocol_version(&self, server_addr: &str, version: f32) -> Result<(), String> {
        debug!(
            "Set protocol version for electrum server: {}, version: {}",
            server_addr, version
        );
        let conn = self
            .connection_manager
            .get_connection_by_address(server_addr)
            .await
            .map_err(|err| err.to_string())?;
        conn.lock().await.set_protocol_version(version).await;

        if let Some(sender) = &self.scripthash_notification_sender {
            sender
                .unbounded_send(ScripthashNotification::RefreshSubscriptions)
                .map_err(|e| ERRL!("Failed sending scripthash message. {}", e))?;
        }

        Ok(())
    }

    /// Get available protocol versions.
    pub fn protocol_version(&self) -> &OrdRange<f32> { &self.protocol_version }

    /// Get block headers storage.
    pub fn block_headers_storage(&self) -> &BlockHeaderStorage { &self.block_headers_storage }

    pub fn weak_spawner(&self) -> WeakSpawner { self.abortable_system.weak_spawner() }

    #[cfg(test)]
    pub(super) fn with_protocol_version(
        client_settings: ElectrumClientSettings,
        protocol_version: OrdRange<f32>,
        block_headers_storage: BlockHeaderStorage,
        abortable_system: AbortableQueue,
        event_sender: futures::channel::mpsc::UnboundedSender<ElectrumClientEvent>,
        scripthash_notification_sender: ScripthashNotificationSender,
    ) -> ElectrumClientImpl {
        ElectrumClientImpl {
            protocol_version,
            ..ElectrumClientImpl::try_new(
                client_settings,
                block_headers_storage,
                abortable_system,
                event_sender,
                scripthash_notification_sender,
            )
            .expect("Expected electrum_client_impl constructed without a problem")
        }
    }
}

#[derive(Clone, Debug)]
pub struct ElectrumClient(pub Arc<ElectrumClientImpl>);

impl Deref for ElectrumClient {
    type Target = ElectrumClientImpl;
    fn deref(&self) -> &ElectrumClientImpl { &self.0 }
}

impl UtxoJsonRpcClientInfo for ElectrumClient {
    fn coin_name(&self) -> &str { self.coin_ticker.as_str() }
}

impl JsonRpcClient for ElectrumClient {
    fn version(&self) -> &'static str { "2.0" }

    fn next_id(&self) -> String { self.next_id.fetch_add(1, AtomicOrdering::Relaxed).to_string() }

    fn client_info(&self) -> String { UtxoJsonRpcClientInfo::client_info(self) }

    fn transport(&self, request: JsonRpcRequestEnum) -> JsonRpcResponseFut {
        Box::new(electrum_request_multi(self.clone(), request).boxed().compat())
    }
}

impl JsonRpcMultiClient for ElectrumClient {
    fn transport_exact(&self, to_addr: String, request: JsonRpcRequestEnum) -> JsonRpcResponseFut {
        Box::new(electrum_request_to(self.clone(), request, to_addr).boxed().compat())
    }
}

impl JsonRpcBatchClient for ElectrumClient {}

#[cfg_attr(test, mockable)]
impl ElectrumClient {
    pub(super) fn try_new(
        client_settings: ElectrumClientSettings,
        mut event_handlers: Vec<Box<dyn RpcTransportEventHandler>>,
        block_headers_storage: BlockHeaderStorage,
        abortable_system: AbortableQueue,
        scripthash_notification_sender: ScripthashNotificationSender,
        spawn_ping: bool,
    ) -> Result<ElectrumClient, String> {
        //let spawner = abortable_system.weak_spawner();
        //let (sender, receiver) = futures::channel::mpsc::unbounded::<ElectrumClientEvent>();
        if let Some(scripthash_notification_sender) = scripthash_notification_sender {
            event_handlers.push(Box::new(ElectrumScriptHashNotificationBridge {
                scripthash_notification_sender,
            }));
        }

        let client = ElectrumClient(Arc::new(ElectrumClientImpl::try_new(
            client_settings,
            block_headers_storage,
            abortable_system,
            //sender,
            scripthash_notification_sender,
        )?));

        // FIXME: Ping inside the connection loop not here
        // no event loop is needed, all should be handled using event handlers.
        // if spwan_ping {
        //     spawner.spawn(client.clone().ping_loop());
        // }
        // spawner.spawn(client.clone().event_loop(receiver, event_handlers));

        client.connect().await?;

        Ok(client)
    }

    async fn event_loop(
        self,
        mut receiver: futures::channel::mpsc::UnboundedReceiver<ElectrumClientEvent>,
        event_handlers: Vec<RpcTransportEventHandlerShared>,
    ) {
        let negotiate_version = self.negotiate_version;
        while let Some(event) = receiver.next().await {
            match event {
                // FIXME: Let's also send script hash notifications through this channel
                ElectrumClientEvent::Connected {
                    address,
                    conn_ready_notifier,
                } => {
                    // FIXME(Related to the one above): Send script hash notifications here (RefreshSubscriptions)
                    if !negotiate_version || check_electrum_server_version(self, address).await {
                        if conn_ready_notifier.send(()).is_err() {
                            error!("Failed to notify connection is ready: {}", address);
                        }
                    } else if let Err(err) = &self.connection_manager.connect().await {
                        error!("Failed to reconnect: {}, {}", address, err);
                    }
                    let _ = event_handlers.on_connected(&address);
                    info!("{address} Connected")
                },
                ElectrumClientEvent::Disconnected { address } => {
                    // Inform the connection manager so it can replace this with another connection.
                    self.connection_manager.on_disconnected(&address).await;
                    let _ = event_handlers.on_disconnected(&address);
                    info!("{address} Disconnected")
                },
                ElectrumClientEvent::IncomingResponse { data_len } => event_handlers.on_incoming_response(data_len),
                ElectrumClientEvent::OutgoingRequest { data_len } => event_handlers.on_outgoing_request(data_len),
            }
        }
    }

    async fn ping_loop(self) {
        loop {
            debug!("ping looop");
            if !self.is_connected().await {
                continue;
            }
            debug!("Ping connected electrum servers");
            if let Err(e) = self.server_ping().compat().await {
                error!("Electrum server ping error: {}", e);
            }

            Timer::sleep(PING_TIMEOUT_SEC).await;
        }
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#server-ping
    pub fn server_ping(&self) -> RpcRes<()> { rpc_func!(self, "server.ping") }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#server-version
    pub fn server_version(
        &self,
        server_address: &str,
        client_name: &str,
        version: &OrdRange<f32>,
    ) -> RpcRes<ElectrumProtocolVersion> {
        let protocol_version: Vec<String> = version.flatten().into_iter().map(|v| format!("{}", v)).collect();
        rpc_func_from!(self, server_address, "server.version", client_name, protocol_version)
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-headers-subscribe
    pub fn get_block_count_from(&self, server_address: &str) -> RpcRes<u64> {
        Box::new(
            rpc_func_from!(self, server_address, BLOCKCHAIN_HEADERS_SUB_ID)
                .map(|r: ElectrumBlockHeader| r.block_height()),
        )
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-block-headers
    pub fn get_block_headers_from(
        &self,
        server_address: &str,
        start_height: u64,
        count: NonZeroU64,
    ) -> RpcRes<ElectrumBlockHeadersRes> {
        rpc_func_from!(self, server_address, "blockchain.block.headers", start_height, count)
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-scripthash-listunspent
    /// It can return duplicates sometimes: https://github.com/artemii235/SuperNET/issues/269
    /// We should remove them to build valid transactions
    pub fn scripthash_list_unspent(&self, hash: &str) -> RpcRes<Vec<ElectrumUnspent>> {
        let request_fut = Box::new(rpc_func!(self, "blockchain.scripthash.listunspent", hash).and_then(
            move |unspents: Vec<ElectrumUnspent>| {
                let mut map: HashMap<(H256Json, u32), bool> = HashMap::new();
                let unspents = unspents
                    .into_iter()
                    .filter(|unspent| match map.entry((unspent.tx_hash, unspent.tx_pos)) {
                        Entry::Occupied(_) => false,
                        Entry::Vacant(e) => {
                            e.insert(true);
                            true
                        },
                    })
                    .collect();
                Ok(unspents)
            },
        ));
        let arc = self.clone();
        let hash = hash.to_owned();
        let fut = async move { arc.list_unspent_concurrent_map.wrap_request(hash, request_fut).await };
        Box::new(fut.boxed().compat())
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-scripthash-listunspent
    /// It can return duplicates sometimes: https://github.com/artemii235/SuperNET/issues/269
    /// We should remove them to build valid transactions.
    /// Please note the function returns `ScriptHashUnspents` elements in the same order in which they were requested.
    pub fn scripthash_list_unspent_batch(&self, hashes: Vec<ElectrumScriptHash>) -> RpcRes<Vec<ScriptHashUnspents>> {
        let requests = hashes
            .iter()
            .map(|hash| rpc_req!(self, "blockchain.scripthash.listunspent", hash));
        Box::new(self.batch_rpc(requests).map(move |unspents: Vec<ScriptHashUnspents>| {
            unspents
                .into_iter()
                .map(|hash_unspents| {
                    hash_unspents
                        .into_iter()
                        .unique_by(|unspent| (unspent.tx_hash, unspent.tx_pos))
                        .collect::<Vec<_>>()
                })
                .collect()
        }))
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-scripthash-get-history
    pub fn scripthash_get_history(&self, hash: &str) -> RpcRes<ElectrumTxHistory> {
        rpc_func!(self, "blockchain.scripthash.get_history", hash)
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-scripthash-get-history
    /// Requests history of the `hashes` in a batch and returns them in the same order they were requested.
    pub fn scripthash_get_history_batch<I>(&self, hashes: I) -> RpcRes<Vec<ElectrumTxHistory>>
    where
        I: IntoIterator<Item = String>,
    {
        let requests = hashes
            .into_iter()
            .map(|hash| rpc_req!(self, "blockchain.scripthash.get_history", hash));
        self.batch_rpc(requests)
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-scripthash-gethistory
    pub fn scripthash_get_balance(&self, hash: &str) -> RpcRes<ElectrumBalance> {
        let arc = self.clone();
        let hash = hash.to_owned();
        let fut = async move {
            let request = rpc_func!(arc, "blockchain.scripthash.get_balance", &hash);
            arc.get_balance_concurrent_map.wrap_request(hash, request).await
        };
        Box::new(fut.boxed().compat())
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-scripthash-gethistory
    /// Requests balances in a batch and returns them in the same order they were requested.
    pub fn scripthash_get_balances<I>(&self, hashes: I) -> RpcRes<Vec<ElectrumBalance>>
    where
        I: IntoIterator<Item = String>,
    {
        let requests = hashes
            .into_iter()
            .map(|hash| rpc_req!(self, "blockchain.scripthash.get_balance", &hash));
        self.batch_rpc(requests)
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-headers-subscribe
    pub fn blockchain_headers_subscribe(&self) -> RpcRes<ElectrumBlockHeader> {
        rpc_func!(self, BLOCKCHAIN_HEADERS_SUB_ID)
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-transaction-broadcast
    pub fn blockchain_transaction_broadcast(&self, tx: BytesJson) -> RpcRes<H256Json> {
        rpc_func!(self, "blockchain.transaction.broadcast", tx)
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-estimatefee
    /// It is recommended to set n_blocks as low as possible.
    /// However, in some cases, n_blocks = 1 leads to an unreasonably high fee estimation.
    /// https://github.com/KomodoPlatform/atomicDEX-API/issues/656#issuecomment-743759659
    pub fn estimate_fee(&self, mode: &Option<EstimateFeeMode>, n_blocks: u32) -> UtxoRpcFut<f64> {
        match mode {
            Some(m) => {
                Box::new(rpc_func!(self, "blockchain.estimatefee", n_blocks, m).map_to_mm_fut(UtxoRpcError::from))
            },
            None => Box::new(rpc_func!(self, "blockchain.estimatefee", n_blocks).map_to_mm_fut(UtxoRpcError::from)),
        }
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-block-header
    pub fn blockchain_block_header(&self, height: u64) -> RpcRes<BytesJson> {
        rpc_func!(self, "blockchain.block.header", height)
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-block-headers
    pub fn blockchain_block_headers(&self, start_height: u64, count: NonZeroU64) -> RpcRes<ElectrumBlockHeadersRes> {
        rpc_func!(self, "blockchain.block.headers", start_height, count)
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-transaction-get-merkle
    pub fn blockchain_transaction_get_merkle(&self, txid: H256Json, height: u64) -> RpcRes<TxMerkleBranch> {
        rpc_func!(self, "blockchain.transaction.get_merkle", txid, height)
    }

    // get_tx_height_from_rpc is costly since it loops through history after requesting the whole history of the script pubkey
    // This method should always be used if the block headers are saved to the DB
    async fn get_tx_height_from_storage(&self, tx: &UtxoTx) -> Result<u64, MmError<GetTxHeightError>> {
        let tx_hash = tx.hash().reversed();
        let blockhash = self.get_verbose_transaction(&tx_hash.into()).compat().await?.blockhash;
        Ok(self
            .block_headers_storage()
            .get_block_height_by_hash(blockhash.into())
            .await?
            .ok_or_else(|| {
                GetTxHeightError::HeightNotFound(format!(
                    "Transaction block header is not found in storage for {}",
                    self.client_impl.coin_ticker
                ))
            })?
            .try_into()?)
    }

    // get_tx_height_from_storage is always preferred to be used instead of this, but if there is no headers in storage (storing headers is not enabled)
    // this function can be used instead
    async fn get_tx_height_from_rpc(&self, tx: &UtxoTx) -> Result<u64, GetTxHeightError> {
        for output in tx.outputs.clone() {
            let script_pubkey_str = hex::encode(electrum_script_hash(&output.script_pubkey));
            if let Ok(history) = self.scripthash_get_history(script_pubkey_str.as_str()).compat().await {
                if let Some(item) = history
                    .into_iter()
                    .find(|item| item.tx_hash.reversed() == H256Json(*tx.hash()) && item.height > 0)
                {
                    return Ok(item.height as u64);
                }
            }
        }
        Err(GetTxHeightError::HeightNotFound(format!(
            "Couldn't find height through electrum for {}",
            self.coin_ticker
        )))
    }

    async fn block_header_from_storage(&self, height: u64) -> Result<BlockHeader, MmError<GetBlockHeaderError>> {
        self.block_headers_storage()
            .get_block_header(height)
            .await?
            .ok_or_else(|| {
                GetBlockHeaderError::Internal(format!("Header not found in storage for {}", self.coin_ticker)).into()
            })
    }

    async fn block_header_from_storage_or_rpc(&self, height: u64) -> Result<BlockHeader, MmError<GetBlockHeaderError>> {
        match self.block_header_from_storage(height).await {
            Ok(h) => Ok(h),
            Err(_) => Ok(deserialize(
                self.blockchain_block_header(height).compat().await?.as_slice(),
            )?),
        }
    }

    pub async fn get_confirmed_tx_info_from_rpc(
        &self,
        tx: &UtxoTx,
    ) -> Result<ConfirmedTransactionInfo, GetConfirmedTxError> {
        let height = self.get_tx_height_from_rpc(tx).await?;

        let merkle_branch = self
            .blockchain_transaction_get_merkle(tx.hash().reversed().into(), height)
            .compat()
            .await?;

        let header = deserialize(self.blockchain_block_header(height).compat().await?.as_slice())?;

        Ok(ConfirmedTransactionInfo {
            tx: tx.clone(),
            header,
            index: merkle_branch.pos as u64,
            height,
        })
    }

    pub async fn get_merkle_and_validated_header(
        &self,
        tx: &UtxoTx,
    ) -> Result<(TxMerkleBranch, BlockHeader, u64), MmError<SPVError>> {
        let height = self.get_tx_height_from_storage(tx).await?;

        let merkle_branch = self
            .blockchain_transaction_get_merkle(tx.hash().reversed().into(), height)
            .compat()
            .await
            .map_to_mm(|err| SPVError::UnableToGetMerkle {
                coin: self.coin_ticker.clone(),
                err: err.to_string(),
            })?;

        let header = self.block_header_from_storage(height).await?;

        Ok((merkle_branch, header, height))
    }

    pub fn retrieve_headers_from(
        &self,
        server_address: &str,
        from_height: u64,
        to_height: u64,
    ) -> UtxoRpcFut<(HashMap<u64, BlockHeader>, Vec<BlockHeader>)> {
        let coin_name = self.coin_ticker.clone();
        if from_height == 0 || to_height < from_height {
            return Box::new(futures01::future::err(
                UtxoRpcError::Internal("Invalid values for from/to parameters".to_string()).into(),
            ));
        }
        let count: NonZeroU64 = match (to_height - from_height + 1).try_into() {
            Ok(c) => c,
            Err(e) => return Box::new(futures01::future::err(UtxoRpcError::Internal(e.to_string()).into())),
        };
        Box::new(
            self.get_block_headers_from(server_address, from_height, count)
                .map_to_mm_fut(UtxoRpcError::from)
                .and_then(move |headers| {
                    let (block_registry, block_headers) = {
                        if headers.count == 0 {
                            return MmError::err(UtxoRpcError::Internal("No headers available".to_string()));
                        }
                        let len = CompactInteger::from(headers.count);
                        let mut serialized = serialize(&len).take();
                        serialized.extend(headers.hex.0.into_iter());
                        drop_mutability!(serialized);
                        let mut reader =
                            Reader::new_with_coin_variant(serialized.as_slice(), coin_name.as_str().into());
                        let maybe_block_headers = reader.read_list::<BlockHeader>();
                        let block_headers = match maybe_block_headers {
                            Ok(headers) => headers,
                            Err(e) => return MmError::err(UtxoRpcError::InvalidResponse(format!("{:?}", e))),
                        };
                        let mut block_registry: HashMap<u64, BlockHeader> = HashMap::new();
                        let mut starting_height = from_height;
                        for block_header in &block_headers {
                            block_registry.insert(starting_height, block_header.clone());
                            starting_height += 1;
                        }
                        (block_registry, block_headers)
                    };
                    Ok((block_registry, block_headers))
                }),
        )
    }

    pub(crate) fn get_servers_with_latest_block_count(&self) -> UtxoRpcFut<(Vec<String>, u64)> {
        let selfi = self.clone();
        let fut = async move {
            // FIXME: Replace this with a `.get_all_addresses` from the connection manager.
            let connections: Vec<ElectrumConnection> = vec![];
            let futures = connections
                .iter()
                .map(|connection| {
                    let addr = connection.addr.clone();
                    selfi
                        .get_block_count_from(&addr)
                        .map(|response| (addr, response))
                        .compat()
                })
                .collect::<Vec<_>>();
            drop(connections);

            let responses = join_all(futures).await;

            // First, we use filter_map to get rid of any errors and collect the
            // server addresses and block counts into two vectors
            let (responding_servers, block_counts_from_all_servers): (Vec<_>, Vec<_>) =
                responses.clone().into_iter().filter_map(|res| res.ok()).unzip();

            // Next, we use max to find the maximum block count from all servers
            if let Some(max_block_count) = block_counts_from_all_servers.clone().iter().max() {
                // Then, we use filter and collect to get the servers that have the maximum block count
                let servers_with_max_count: Vec<_> = responding_servers
                    .into_iter()
                    .zip(block_counts_from_all_servers)
                    .filter(|(_, count)| count == max_block_count)
                    .map(|(addr, _)| addr)
                    .collect();

                // Finally, we return a tuple of servers with max count and the max count
                return Ok((servers_with_max_count, *max_block_count));
            }

            return Err(MmError::new(UtxoRpcError::Internal(format!(
                "Couldn't get block count from any server for {}, responses: {:?}",
                &selfi.coin_ticker, responses
            ))));
        };

        Box::new(fut.boxed().compat())
    }
}

// If mockable is placed before async_trait there is `munmap_chunk(): invalid pointer` error on async fn mocking attempt
#[async_trait]
#[cfg_attr(test, mockable)]
impl UtxoRpcClientOps for ElectrumClient {
    fn list_unspent(&self, address: &Address, _decimals: u8) -> UtxoRpcFut<Vec<UnspentInfo>> {
        let script = try_f!(output_script(address));
        let script_hash = electrum_script_hash(&script);
        Box::new(
            self.scripthash_list_unspent(&hex::encode(script_hash))
                .map_to_mm_fut(UtxoRpcError::from)
                .map(move |unspents| {
                    unspents
                        .iter()
                        .map(|unspent| UnspentInfo {
                            outpoint: OutPoint {
                                hash: unspent.tx_hash.reversed().into(),
                                index: unspent.tx_pos,
                            },
                            value: unspent.value,
                            height: unspent.height,
                        })
                        .collect()
                }),
        )
    }

    fn list_unspent_group(&self, addresses: Vec<Address>, _decimals: u8) -> UtxoRpcFut<UnspentMap> {
        let script_hashes = try_f!(addresses
            .iter()
            .map(|addr| {
                let script = output_script(addr)?;
                let script_hash = electrum_script_hash(&script);
                Ok(hex::encode(script_hash))
            })
            .collect::<Result<Vec<_>, keys::Error>>());

        let this = self.clone();
        let fut = async move {
            let unspents = this.scripthash_list_unspent_batch(script_hashes).compat().await?;

            let unspent_map = addresses
                .into_iter()
                // `scripthash_list_unspent_batch` returns `ScriptHashUnspents` elements in the same order in which they were requested.
                // So we can zip `addresses` and `unspents` into one iterator.
                .zip(unspents)
                // Map `(Address, Vec<ElectrumUnspent>)` pairs into `(Address, Vec<UnspentInfo>)`.
                .map(|(address, electrum_unspents)| (address, electrum_unspents.collect_into()))
                .collect();
            Ok(unspent_map)
        };
        Box::new(fut.boxed().compat())
    }

    fn send_transaction(&self, tx: &UtxoTx) -> UtxoRpcFut<H256Json> {
        let bytes = if tx.has_witness() {
            BytesJson::from(serialize_with_flags(tx, SERIALIZE_TRANSACTION_WITNESS))
        } else {
            BytesJson::from(serialize(tx))
        };
        Box::new(
            self.blockchain_transaction_broadcast(bytes)
                .map_to_mm_fut(UtxoRpcError::from),
        )
    }

    fn send_raw_transaction(&self, tx: BytesJson) -> UtxoRpcFut<H256Json> {
        Box::new(
            self.blockchain_transaction_broadcast(tx)
                .map_to_mm_fut(UtxoRpcError::from),
        )
    }

    fn blockchain_scripthash_subscribe(&self, scripthash: String) -> UtxoRpcFut<Json> {
        Box::new(rpc_func!(self, BLOCKCHAIN_SCRIPTHASH_SUB_ID, scripthash).map_to_mm_fut(UtxoRpcError::from))
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-transaction-get
    /// returns transaction bytes by default
    fn get_transaction_bytes(&self, txid: &H256Json) -> UtxoRpcFut<BytesJson> {
        let verbose = false;
        Box::new(rpc_func!(self, "blockchain.transaction.get", txid, verbose).map_to_mm_fut(UtxoRpcError::from))
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-transaction-get
    /// returns verbose transaction by default
    fn get_verbose_transaction(&self, txid: &H256Json) -> UtxoRpcFut<RpcTransaction> {
        let verbose = true;
        Box::new(rpc_func!(self, "blockchain.transaction.get", txid, verbose).map_to_mm_fut(UtxoRpcError::from))
    }

    /// https://electrumx.readthedocs.io/en/latest/protocol-methods.html#blockchain-transaction-get
    /// Returns verbose transactions in a batch.
    fn get_verbose_transactions(&self, tx_ids: &[H256Json]) -> UtxoRpcFut<Vec<RpcTransaction>> {
        let verbose = true;
        let requests = tx_ids
            .iter()
            .map(|txid| rpc_req!(self, "blockchain.transaction.get", txid, verbose));
        Box::new(self.batch_rpc(requests).map_to_mm_fut(UtxoRpcError::from))
    }

    fn get_block_count(&self) -> UtxoRpcFut<u64> {
        Box::new(
            self.blockchain_headers_subscribe()
                .map(|r| r.block_height())
                .map_to_mm_fut(UtxoRpcError::from),
        )
    }

    fn display_balance(&self, address: Address, decimals: u8) -> RpcRes<BigDecimal> {
        let output_script = try_f!(output_script(&address).map_err(|err| JsonRpcError::new(
            UtxoJsonRpcClientInfo::client_info(self),
            rpc_req!(self, "blockchain.scripthash.get_balance").into(),
            JsonRpcErrorType::Internal(err.to_string())
        )));
        let hash = electrum_script_hash(&output_script);
        let hash_str = hex::encode(hash);
        Box::new(
            self.scripthash_get_balance(&hash_str)
                .map(move |electrum_balance| electrum_balance.to_big_decimal(decimals)),
        )
    }

    fn display_balances(&self, addresses: Vec<Address>, decimals: u8) -> UtxoRpcFut<Vec<(Address, BigDecimal)>> {
        let this = self.clone();
        let fut = async move {
            let hashes = addresses
                .iter()
                .map(|address| {
                    let output_script = output_script(address)?;
                    let hash = electrum_script_hash(&output_script);

                    Ok(hex::encode(hash))
                })
                .collect::<Result<Vec<_>, keys::Error>>()?;

            let electrum_balances = this.scripthash_get_balances(hashes).compat().await?;
            let balances = electrum_balances
                .into_iter()
                // `scripthash_get_balances` returns `ElectrumBalance` elements in the same order in which they were requested.
                // So we can zip `addresses` and the balances into one iterator.
                .zip(addresses)
                .map(|(electrum_balance, address)| (address, electrum_balance.to_big_decimal(decimals)))
                .collect();
            Ok(balances)
        };

        Box::new(fut.boxed().compat())
    }

    fn estimate_fee_sat(
        &self,
        decimals: u8,
        _fee_method: &EstimateFeeMethod,
        mode: &Option<EstimateFeeMode>,
        n_blocks: u32,
    ) -> UtxoRpcFut<u64> {
        Box::new(self.estimate_fee(mode, n_blocks).map(move |fee| {
            if fee > 0.00001 {
                (fee * 10.0_f64.powf(decimals as f64)) as u64
            } else {
                1000
            }
        }))
    }

    fn get_relay_fee(&self) -> RpcRes<BigDecimal> { rpc_func!(self, "blockchain.relayfee") }

    fn find_output_spend(
        &self,
        tx_hash: H256,
        script_pubkey: &[u8],
        vout: usize,
        _from_block: BlockHashOrHeight,
        tx_hash_algo: TxHashAlgo,
    ) -> Box<dyn Future<Item = Option<SpentOutputInfo>, Error = String> + Send> {
        let selfi = self.clone();
        let script_hash = hex::encode(electrum_script_hash(script_pubkey));
        let fut = async move {
            let history = try_s!(selfi.scripthash_get_history(&script_hash).compat().await);

            if history.len() < 2 {
                return Ok(None);
            }

            for item in history.iter() {
                let transaction = try_s!(selfi.get_transaction_bytes(&item.tx_hash).compat().await);

                let mut maybe_spend_tx: UtxoTx =
                    try_s!(deserialize(transaction.as_slice()).map_err(|e| ERRL!("{:?}", e)));
                maybe_spend_tx.tx_hash_algo = tx_hash_algo;
                drop_mutability!(maybe_spend_tx);

                for (index, input) in maybe_spend_tx.inputs.iter().enumerate() {
                    if input.previous_output.hash == tx_hash && input.previous_output.index == vout as u32 {
                        return Ok(Some(SpentOutputInfo {
                            input: input.clone(),
                            input_index: index,
                            spending_tx: maybe_spend_tx,
                            spent_in_block: BlockHashOrHeight::Height(item.height),
                        }));
                    }
                }
            }
            Ok(None)
        };
        Box::new(fut.boxed().compat())
    }

    fn get_median_time_past(
        &self,
        starting_block: u64,
        count: NonZeroU64,
        coin_variant: CoinVariant,
    ) -> UtxoRpcFut<u32> {
        let from = if starting_block <= count.get() {
            0
        } else {
            starting_block - count.get() + 1
        };
        Box::new(
            self.blockchain_block_headers(from, count)
                .map_to_mm_fut(UtxoRpcError::from)
                .and_then(|res| {
                    if res.count == 0 {
                        return MmError::err(UtxoRpcError::InvalidResponse("Server returned zero count".to_owned()));
                    }
                    let len = CompactInteger::from(res.count);
                    let mut serialized = serialize(&len).take();
                    serialized.extend(res.hex.0.into_iter());
                    let mut reader = Reader::new_with_coin_variant(serialized.as_slice(), coin_variant);
                    let headers = reader.read_list::<BlockHeader>()?;
                    let mut timestamps: Vec<_> = headers.into_iter().map(|block| block.time).collect();
                    // can unwrap because count is non zero
                    Ok(median(timestamps.as_mut_slice()).unwrap())
                }),
        )
    }

    async fn get_block_timestamp(&self, height: u64) -> Result<u64, MmError<GetBlockHeaderError>> {
        Ok(self.block_header_from_storage_or_rpc(height).await?.time as u64)
    }
}
