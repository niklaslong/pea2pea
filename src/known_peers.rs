use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use fxhash::FxHashMap;
use std::{net::SocketAddr, time::Instant};

/// Contains statistics related to node's peers, currently connected or not.
#[derive(Default)]
pub struct KnownPeers(RwLock<FxHashMap<SocketAddr, PeerStats>>);

impl KnownPeers {
    /// Adds an address to the list of known peers.
    pub fn add(&self, addr: SocketAddr) {
        self.write().entry(addr).or_default().new_connection();
    }

    /// Removes an address to the list of known peers.
    pub fn remove(&self, addr: SocketAddr) -> Option<PeerStats> {
        self.write().remove(&addr)
    }

    /// Registers a connection to the given address, adding a peer if it hasn't been known.
    pub fn register_connection(&self, addr: SocketAddr) {
        self.write().entry(addr).or_default().new_connection();
    }

    /// Updates the "last seen" timestamp of the given address, adding a peer if it hasn't been known.
    pub fn update_last_seen(&self, addr: SocketAddr) {
        self.write().entry(addr).or_default().update_last_seen();
    }

    /// Registers a submission of a message to the given address, adding a peer if it hasn't been known.
    pub fn register_sent_message(&self, to: SocketAddr, len: usize) {
        self.write().entry(to).or_default().sent_message(len);
    }

    /// Registers a receipt of a message to the given address, adding a peer if it hasn't been known.
    pub fn register_received_message(&self, from: SocketAddr, len: usize) {
        self.write().entry(from).or_default().received_message(len);
    }

    /// Registers a failure associated with the given address, adding a peer if it hasn't been known.
    pub fn register_failure(&self, addr: SocketAddr) {
        self.write().entry(addr).or_default().register_failure();
    }

    /// Acquires a read lock over the collection of known peers.
    pub fn read(&self) -> RwLockReadGuard<'_, FxHashMap<SocketAddr, PeerStats>> {
        self.0.read()
    }

    /// Acquires a write lock over the collection of known peers.
    pub fn write(&self) -> RwLockWriteGuard<'_, FxHashMap<SocketAddr, PeerStats>> {
        self.0.write()
    }
}

/// Contains statistics related to a single peer.
#[derive(Debug, Clone)]
pub struct PeerStats {
    /// The number of times a connection with the peer has been established.
    pub times_connected: usize,
    /// The timestamp of the first connection with the peer.
    pub first_seen: Instant,
    /// The timestamp of the current connection with the peer.
    pub last_connected: Instant,
    /// The timestamp of the peer's last activity.
    pub last_seen: Instant,
    /// The number of messages sent to the peer.
    pub msgs_sent: usize,
    /// The number of messages received from the peer.
    pub msgs_received: usize,
    /// The number of bytes sent to the peer.
    pub bytes_sent: u64,
    /// The number of bytes received from the peer.
    pub bytes_received: u64,
    /// The number of failures related to the peer.
    pub failures: u8,
}

impl Default for PeerStats {
    fn default() -> Self {
        let now = Instant::now();

        Self {
            times_connected: 1,
            first_seen: now,
            last_connected: now,
            last_seen: now,
            msgs_sent: 0,
            msgs_received: 0,
            bytes_sent: 0,
            bytes_received: 0,
            failures: 0,
        }
    }
}

impl PeerStats {
    pub(crate) fn update_last_seen(&mut self) {
        // last_seen is not updated automatically, as many messages can be read
        // from the stream at once, and each one would cause a costly system call;
        // since it is a costly operation, it is opt-in; it can be included in
        // one of the methods provided by the Messaging protocol
        self.last_seen = Instant::now();
    }

    pub(crate) fn new_connection(&mut self) {
        self.last_connected = Instant::now();
        self.times_connected += 1;
    }

    pub(crate) fn sent_message(&mut self, msg_len: usize) {
        self.msgs_sent += 1;
        self.bytes_sent += msg_len as u64;
    }

    pub(crate) fn received_message(&mut self, msg_len: usize) {
        self.msgs_received += 1;
        self.bytes_received += msg_len as u64;
    }

    pub(crate) fn register_failure(&mut self) {
        self.failures += 1;
    }
}
