//! Server-wide metrics — connection counts, command throughput, keyspace stats.
//!
//! All counters use `AtomicU64` with `Ordering::Relaxed` for throughput; exact
//! consistency is not required since INFO is sampled at human timescales.
//!
//! Exposed via a process-global `OnceLock<Arc<ServerMetrics>>` so that
//! `info.rs`, `db.rs`, and `main.rs` can all reach the same instance without
//! threading the object through every call-site parameter list.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static METRICS: OnceLock<Arc<ServerMetrics>> = OnceLock::new();

/// Install the global metrics instance. Must be called once at server startup
/// before any connection is accepted. Subsequent calls return the existing
/// instance unchanged.
pub fn server_metrics() -> &'static Arc<ServerMetrics> {
    METRICS.get_or_init(|| {
        let start_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Arc::new(ServerMetrics::new(start_ms))
    })
}

/// Atomically tracked server-wide counters.
pub struct ServerMetrics {
    /// Unix milliseconds when the server process started.
    pub start_time_ms: u64,
    /// Number of clients whose TCP session is currently open.
    pub connected_clients: AtomicU64,
    /// Peak value of `connected_clients` ever observed.
    pub max_clients_seen: AtomicU64,
    /// Total accepted connections since startup.
    pub total_connections_received: AtomicU64,
    /// Total commands dispatched since startup.
    pub total_commands_processed: AtomicU64,
    /// Successful key lookups (key found, not expired).
    pub keyspace_hits: AtomicU64,
    /// Failed key lookups (key absent or expired).
    pub keyspace_misses: AtomicU64,
    /// Connections rejected because connected_clients >= maxclients.
    pub rejected_connections: AtomicU64,
    /// Keys removed by lazy or active expiration.
    pub expired_keys: AtomicU64,
}

impl ServerMetrics {
    fn new(start_time_ms: u64) -> Self {
        Self {
            start_time_ms,
            connected_clients: AtomicU64::new(0),
            max_clients_seen: AtomicU64::new(0),
            total_connections_received: AtomicU64::new(0),
            total_commands_processed: AtomicU64::new(0),
            keyspace_hits: AtomicU64::new(0),
            keyspace_misses: AtomicU64::new(0),
            rejected_connections: AtomicU64::new(0),
            expired_keys: AtomicU64::new(0),
        }
    }

    /// Increment `connected_clients`, updating `max_clients_seen` if the new
    /// value exceeds the recorded peak.
    pub fn on_connect(&self) {
        let prev = self.connected_clients.fetch_add(1, Ordering::Relaxed);
        let new = prev + 1;
        let mut peak = self.max_clients_seen.load(Ordering::Relaxed);
        while new > peak {
            match self.max_clients_seen.compare_exchange_weak(
                peak,
                new,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => peak = current,
            }
        }
    }

    /// Decrement `connected_clients` on disconnect.
    pub fn on_disconnect(&self) {
        self.connected_clients.fetch_sub(1, Ordering::Relaxed);
    }
}
