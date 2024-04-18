enum ElectrumProtoVerifierEvent {
    Connected(String),
    Disconnected(String),
}

/// Electrum protocol version verifier.
/// The structure is used to handle the `on_connected` event and notify `electrum_version_loop`.
struct ElectrumProtoVerifier {
    on_event_tx: UnboundedSender<ElectrumProtoVerifierEvent>,
}

impl RpcTransportEventHandler for ElectrumProtoVerifier {
    fn debug_info(&self) -> String { "ElectrumProtoVerifier".into() }

    fn on_outgoing_request(&self, _data_len: usize) {}

    fn on_incoming_response(&self, _data_len: usize) {}

    fn on_connected(&self, address: &str) -> Result<(), String> {
        debug!("Connected to the electrum server: {}", address);
        try_s!(self
            .on_event_tx
            .unbounded_send(ElectrumProtoVerifierEvent::Connected(address.to_string())));
        Ok(())
    }

    fn on_disconnected(&self, address: &str) -> Result<(), String> {
        debug!("Disconnected from the electrum server: {}", address);
        try_s!(self
            .on_event_tx
            .unbounded_send(ElectrumProtoVerifierEvent::Disconnected(address.to_string())));
        Ok(())
    }
}
