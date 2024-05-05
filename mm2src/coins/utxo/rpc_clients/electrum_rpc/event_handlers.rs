use super::connection_managers::ConnectionManagerTrait;
use super::constants::BLOCKCHAIN_SCRIPTHASH_SUB_ID;
use crate::utxo::ScripthashNotification;
use crate::{big_decimal_from_sat_unsigned, NumConversError, RpcTransportEventHandler, RpcTransportEventHandlerShared};
use common::jsonrpc_client::{JsonRpcBatchClient, JsonRpcBatchResponse, JsonRpcClient, JsonRpcError, JsonRpcErrorType,
                             JsonRpcId, JsonRpcMultiClient, JsonRpcRemoteAddr, JsonRpcRequest, JsonRpcRequestEnum,
                             JsonRpcResponse, JsonRpcResponseEnum, JsonRpcResponseFut, RpcRes};
use common::log::{debug, error, info, warn};
use futures::channel::mpsc::{Receiver as AsyncReceiver, Sender as AsyncSender, UnboundedReceiver, UnboundedSender};
use serde_json::{self as json, Value as Json};

/// An `RpcTransportEventHandler` that forwards `ScripthashNotification`s to trigger balance updates.
///
/// This handler hooks in `on_incoming_response` and looks for an electrum script hash notification to forward it.
pub struct ElectrumScriptHashNotificationBridge {
    pub scripthash_notification_sender: UnboundedSender<ScripthashNotification>,
}

impl RpcTransportEventHandler for ElectrumScriptHashNotificationBridge {
    fn debug_info(&self) -> String { "ElectrumScriptHashNotificationBridge".into() }

    fn on_incoming_response(&self, data: &[u8]) {
        if let Ok(raw_json) = json::from_slice::<Json>(data) {
            // Try to parse the notification. A notification is sent as a JSON-RPC request.
            if let Ok(notification) = json::from_value::<JsonRpcRequest>(raw_json) {
                // Only care about `BLOCKCHAIN_SCRIPTHASH_SUB_ID` notifications.
                if notification.method.as_str() == BLOCKCHAIN_SCRIPTHASH_SUB_ID {
                    if let Some(scripthash) = notification.params.first().map(|s| s.as_str()).flatten() {
                        if let Err(e) = self
                            .scripthash_notification_sender
                            .unbounded_send(ScripthashNotification::Triggered(scripthash.to_string()))
                        {
                            error!("Failed sending script hash message. {e:?}");
                        }
                    } else {
                        warn!("Notification must contain the script hash value, got: {notification:?}");
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
pub struct ElectrumConnectionManagerNotifier {
    pub connection_manager: Box<dyn ConnectionManagerTrait>,
}

impl RpcTransportEventHandler for ElectrumConnectionManagerNotifier {
    fn debug_info(&self) -> String { "ElectrumConnectionManagerNotifier".into() }

    fn on_connected(&self, address: &str) -> Result<(), String> {
        self.connection_manager.on_connected(address);
        Ok(())
    }

    fn on_disconnected(&self, address: &str) -> Result<(), String> {
        self.connection_manager.on_disconnected(address);
        Ok(())
    }
}
