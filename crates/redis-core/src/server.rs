//! `RedisServer` — global server state.
//!
//! STUB. Just enough surface for command implementations to reach a
//! database, the next client id, and a few config knobs. Replication,
//! cluster, persistence, modules — all deferred to their own phases.

use crate::db::RedisDb;
use crate::client::ClientId;
use crate::evict::EvictionPool;

/// Stub AOF state. TODO(architect): real type lives in `aof.rs` when ported
/// (Phase 6+). Until then this is an i32 discriminant matching the C
/// `AOF_OFF`/`AOF_ON`/`AOF_WAIT_REWRITE` constants.
pub type AofState = i32;

pub const AOF_OFF: AofState = 0;
pub const AOF_ON: AofState = 1;
pub const AOF_WAIT_REWRITE: AofState = 2;

/// Stub command table handle.
///
/// TODO(architect): real type later — should be a reference to the registry
/// in `redis-commands::generated::COMMANDS` plus a `HashMap<&[u8], &spec>`
/// case-insensitive lookup. The Wave A pilot does the lookup directly via
/// `redis_commands::dispatch::lookup_command` and does not store a handle on
/// the server.
#[derive(Debug, Default, Clone, Copy)]
pub struct CommandTableHandle;

/// Stub listener handle. TODO(architect): real type later (collapse with
/// `connection::ConnListener` once the vtable registry has live backends).
#[derive(Debug, Default)]
pub struct ListenerHandle {
    /// Number of bound file descriptors (0 when the listener is inactive).
    pub fd_count: i32,
}

#[derive(Debug)]
pub struct RedisServer {
    /// Tick counter for assigning client ids.
    next_client_id: ClientId,
    /// Databases. Standalone defaults to 16 dbs; pilot uses just 1.
    dbs: Vec<RedisDb>,
    /// Bind port (configured at startup).
    pub port: u16,
    /// Bind addresses as raw bytes (e.g. `b"127.0.0.1"`).
    pub bind_addrs: Vec<Vec<u8>>,
    /// Single-source-of-truth config flags (more land later).
    pub config: ServerConfig,
    /// LRU/LFU eviction candidate pool.
    /// C: static struct evictionPoolEntry *EvictionPoolLRU — evict.c:64
    /// TODO(port): initialise via eviction_pool_alloc() in server startup path.
    pub eviction_pool: EvictionPool,
    /// Command-table handle. TODO(architect): real type later.
    pub commands_table: CommandTableHandle,
    /// Server event-loop frequency (Hz). C: `server.hz`.
    pub hz: i32,
    /// AOF state. TODO(architect): real `AofState` enum later.
    pub aof_state: AofState,
    /// Cached command-time snapshot in milliseconds since epoch.
    /// C: `server.cmd_time_snapshot`.
    pub cmd_time_snapshot: i64,
    /// Active TCP listeners. TODO(architect): real type later (Vec of
    /// `connection::ConnListener` once backends register).
    pub listeners: Vec<ListenerHandle>,
    /// Number of clients currently in a MULTI block watching keys.
    /// C: `server.watching_clients`.
    pub watching_clients: u64,
    /// Dirty counter — increments per write command for AOF/replication.
    /// C: `server.dirty`.
    pub dirty: i64,
    /// Whether the server is in the middle of an EXEC dispatch.
    /// C: `server.in_exec`.
    pub in_exec: bool,
    /// Whether the server is paused (CLIENT PAUSE / failover).
    /// C: `server.pause_cron`.
    pub pause_cron: bool,
    /// Maximum size of a bulk reply payload in bytes.
    /// C: `server.proto_max_bulk_len`.
    pub proto_max_bulk_len: i64,
    /// Server start time (Unix milliseconds).
    pub start_time_ms: i64,
    /// Shutdown flag — checked by the event loop and accept loop.
    /// C: `server.shutdown_asap`.
    pub shutdown_asap: bool,
}

/// Default value of `server.hz` (events per second).
pub const CONFIG_DEFAULT_HZ: i32 = 10;

/// Default value of `server.proto_max_bulk_len` (512 MiB).
pub const PROTO_MAX_BULK_LEN_DEFAULT: i64 = 512 * 1024 * 1024;

#[derive(Debug, Default, Clone)]
pub struct ServerConfig {
    /// `--maxmemory` equivalent (bytes; 0 = unlimited).
    pub max_memory: u64,
    /// Whether DEBUG command is enabled.
    pub enable_debug_command: bool,
}

impl Default for RedisServer {
    fn default() -> Self {
        Self::new(6379)
    }
}

impl RedisServer {
    pub fn new(port: u16) -> Self {
        use crate::evict::eviction_pool_alloc;
        Self {
            next_client_id: 0,
            dbs: vec![RedisDb::new(0)],
            port,
            bind_addrs: Vec::new(),
            config: ServerConfig::default(),
            eviction_pool: eviction_pool_alloc(),
            commands_table: CommandTableHandle,
            hz: CONFIG_DEFAULT_HZ,
            aof_state: AOF_OFF,
            cmd_time_snapshot: 0,
            listeners: Vec::new(),
            watching_clients: 0,
            dirty: 0,
            in_exec: false,
            pause_cron: false,
            proto_max_bulk_len: PROTO_MAX_BULK_LEN_DEFAULT,
            start_time_ms: 0,
            shutdown_asap: false,
        }
    }

    pub fn alloc_client_id(&mut self) -> ClientId {
        let id = self.next_client_id;
        self.next_client_id = self.next_client_id.wrapping_add(1);
        id
    }

    pub fn db(&self, index: u32) -> Option<&RedisDb> {
        self.dbs.get(index as usize)
    }

    pub fn db_mut(&mut self, index: u32) -> Option<&mut RedisDb> {
        self.dbs.get_mut(index as usize)
    }

    pub fn db_count(&self) -> usize {
        self.dbs.len()
    }

    /// Add additional databases (standalone Redis defaults to 16).
    pub fn set_db_count(&mut self, n: usize) {
        while self.dbs.len() < n {
            let id = self.dbs.len() as u32;
            self.dbs.push(RedisDb::new(id));
        }
        self.dbs.truncate(n);
    }

    /// Whether cluster mode is enabled (maps to C `server.cluster_enabled`).
    ///
    /// STUB — Phase B placeholder; cluster wiring is Phase 3+.
    pub fn cluster_enabled(&self) -> bool {
        false
    }

    /// Maximum idle time, in seconds, before an idle client is closed
    /// (maps to C `server.maxidletime`).
    ///
    /// STUB — Phase B placeholder returning 0 (disabled). Real value comes
    /// from CONFIG once config.c is fully wired.
    pub fn max_idle_time(&self) -> i64 {
        0
    }

    /// Set the server-wide `in_exec` flag (true while EXEC is mid-flight).
    pub fn set_in_exec(&mut self, value: bool) {
        self.in_exec = value;
    }
}

// ──────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        architect packet (stub for translate-loop unblock)
//   target_crate:  redis-core
//   confidence:    skeleton
//   todos:         0
//   port_notes:    1
//   unsafe_blocks: 0
//   notes:         Minimal global state. Replication/cluster/persist/modules deferred to their phases.
// ──────────────────────────────────────────────────────────────────────
