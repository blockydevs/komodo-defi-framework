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
    scripthash_notification_sender: UnboundedSender<ScripthashNotification>,
}

impl RpcTransportEventHandler for ElectrumScriptHashNotificationBridge {
    fn debug_info(&self) -> String { "ElectrumScriptHashNotificationBridge".into() }

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
}

/// An `RpcTransportEventHandler` that notifies the `ConnectionManager` upon connections and  disconnections.
///
/// When a connection is connected or disconnected, this event handler will notify the `ConnectionManager`
/// to handle the the event.
struct ElectrumConnectionManagerNotifier {
    connection_manager: Box<dyn ConnectionManagerTrait + Send + Sync>,
}

impl RpcTransportEventHandler for ElectrumManagerNotifier {
    fn debug_info(&self) -> String { "ElectrumManagerNotifier".into() }

    fn on_connected(&self, address: &str) -> Result<(), String> {
        self.connection_manager.on_connected(address);
        Ok(())
    }

    fn on_disconnected(&self, address: &str) -> Result<(), String> {
        self.connection_manager.on_disconnected(address);
        Ok(())
    }
}
