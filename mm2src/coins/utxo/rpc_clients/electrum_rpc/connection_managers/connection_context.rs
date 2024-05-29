use std::mem;
use std::sync::{Arc, Mutex};

use super::super::connection::{ElectrumConnection};
use super::super::constants::{FIRST_SUSPEND_TIME};

use keys::Address;

use gstuff::now_ms;

/// A struct that encapsulates an Electrum connection and its information.
#[derive(Debug)]
pub(super) struct ConnectionContext {
    /// The electrum connection.
    pub(super) connection: Arc<ElectrumConnection>,
    /// The list of addresses subscribed to the connection.
    subs: Mutex<Vec<Address>>,
    /// How long to suspend the server the next time it disconnects (in milliseconds).
    next_suspend_time: Mutex<u64>,
    /// When was the connection last disconnected.
    disconnected_at: Mutex<u64>,
}

impl ConnectionContext {
    /// Creates a new connection context.
    pub(super) fn new(connection: ElectrumConnection) -> Self {
        ConnectionContext {
            connection: Arc::new(connection),
            subs: Mutex::new(Vec::new()),
            next_suspend_time: Mutex::new(FIRST_SUSPEND_TIME),
            disconnected_at: Mutex::new(0),
        }
    }

    /// Resets the suspend time and disconnection time.
    pub(super) fn connected(&self) {
        *self.disconnected_at.lock().unwrap() = 0;
        *self.next_suspend_time.lock().unwrap() = FIRST_SUSPEND_TIME;
    }

    /// Inform the connection context that the connection has been disconnected.
    ///
    /// Doubles the suspend time and sets the disconnection time to `now`.
    /// Also clears the subs list and returns it.
    pub(super) fn disconnected(&self) -> Vec<Address> {
        // The max time to suspend a server, 12h.
        const MAX_SUSPEND_TIME: u64 = 12 * 60 * 60;
        *self.disconnected_at.lock().unwrap() = now_ms();
        let mut next_suspend_time = self.next_suspend_time.lock().unwrap();
        *next_suspend_time = (*next_suspend_time * 2).min(MAX_SUSPEND_TIME);
        mem::take(&mut self.subs.lock().unwrap())
    }

    /// Returns the time the server should be suspended until (when to take it up) in milliseconds.
    pub(super) fn suspend_until(&self) -> u64 {
        (*self.disconnected_at.lock().unwrap() + *self.next_suspend_time.lock().unwrap()) * 1000
    }

    /// Adds a subscription to the connection context.
    pub(super) fn add_sub(&self, address: Address) { self.subs.lock().unwrap().push(address); }
}
