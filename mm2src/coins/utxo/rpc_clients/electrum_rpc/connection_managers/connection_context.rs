use std::collections::HashSet;
use std::mem;
use std::sync::{Arc, Mutex};

use super::super::connection::ElectrumConnection;
use super::super::constants::FIRST_SUSPEND_TIME;

use common::now_ms;
use keys::Address;

#[derive(Debug)]
struct SuspendTimer {
    /// How long to suspend the server the next time it disconnects (in milliseconds).
    next_suspend_time: u64,
    /// When was the connection last disconnected.
    disconnected_at: u64,
}

impl SuspendTimer {
    /// Creates a new suspend timer.
    fn new() -> Self {
        SuspendTimer {
            next_suspend_time: FIRST_SUSPEND_TIME,
            disconnected_at: 0,
        }
    }

    /// Resets the suspend time and disconnection time.
    fn reset(&mut self) {
        self.disconnected_at = 0;
        self.next_suspend_time = FIRST_SUSPEND_TIME;
    }

    /// Doubles the suspend time and sets the disconnection time to `now`.
    fn double(&mut self) {
        // The max suspend time, 12h.
        const MAX_SUSPEND_TIME: u64 = 12 * 60 * 60;
        self.disconnected_at = now_ms();
        self.next_suspend_time = (self.next_suspend_time * 2).min(MAX_SUSPEND_TIME);
    }

    /// Returns the time until when the server should be suspended in milliseconds.
    fn get_suspend_until(&self) -> u64 { self.disconnected_at + self.next_suspend_time * 1000 }
}

/// A struct that encapsulates an Electrum connection and its information.
#[derive(Debug)]
pub struct ConnectionContext {
    /// The electrum connection.
    pub connection: Arc<ElectrumConnection>,
    /// The list of addresses subscribed to the connection.
    subs: Mutex<HashSet<Address>>,
    /// The timer deciding when the connection is ready to be used again.
    suspend_timer: Mutex<SuspendTimer>,
}

impl ConnectionContext {
    /// Creates a new connection context.
    pub(super) fn new(connection: ElectrumConnection) -> Self {
        ConnectionContext {
            connection: Arc::new(connection),
            subs: Mutex::new(HashSet::new()),
            suspend_timer: Mutex::new(SuspendTimer::new()),
        }
    }

    /// Resets the suspend time.
    pub(super) fn connected(&self) { self.suspend_timer.lock().unwrap().reset(); }

    /// Inform the connection context that the connection has been disconnected.
    ///
    /// Doubles the suspend time and clears the subs list and returns it.
    pub(super) fn disconnected(&self) -> HashSet<Address> {
        self.suspend_timer.lock().unwrap().double();
        mem::take(&mut self.subs.lock().unwrap())
    }

    /// Returns the time the server should be suspended until (when to take it up) in milliseconds.
    pub(super) fn suspend_until(&self) -> u64 { self.suspend_timer.lock().unwrap().get_suspend_until() }

    /// Adds a subscription to the connection context.
    pub(super) fn add_sub(&self, address: Address) { self.subs.lock().unwrap().insert(address); }
}
