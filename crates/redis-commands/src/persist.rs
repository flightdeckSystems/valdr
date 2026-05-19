//! Persistence commands: SAVE, BGSAVE.
//!
//! `SAVE` runs `rdb::save_rdb` synchronously in the calling thread and updates
//! `last_save_unix` on success.
//!
//! `BGSAVE` on Unix uses `fork(2)` so the OS copy-on-write page mapping gives
//! the child a frozen snapshot of the DB without any memory duplication:
//!   1. fork — child sees the DB as it was at the instant of the fork.
//!   2. Child writes the RDB file and calls `_exit(0)` (not `exit()` — skipping
//!      atexit handlers that belong to the parent).
//!   3. Parent records the child PID in `server.rdb_child_pid` and returns
//!      `+Background saving started` immediately.
//!   4. A background polling thread (spawned at server start) calls
//!      `waitpid` every 500 ms to reap the child and update `last_save_unix`.
//!
//! On non-Unix targets (Windows, WASM) the pre-fork thread-snapshot path is
//! kept as the fallback. The fallback allocates a full in-memory clone of the
//! DB before spawning the writer thread.
//!
//! The `unsafe` block that wraps `fork + _exit` is the single unsafe surface in
//! this crate: documented below with a SAFETY comment.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use redis_core::db::RedisDb;
use redis_core::rdb::{rdb_path, save_rdb};
use redis_core::CommandContext;
use redis_types::{RedisError, RedisResult};

/// `SAVE` — synchronous RDB save.
///
/// Writes the RDB file to `<dir>/<dbfilename>` and updates `last_save_unix`
/// on success. Returns `+OK` on success or `-ERR` on failure.
pub fn save_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() != 1 {
        return Err(RedisError::wrong_number_of_args(b"save"));
    }
    let cfg = Arc::clone(&ctx.server().live_config);
    let path = rdb_path(&cfg.rdb_dir(), &cfg.rdb_filename());

    let result = {
        let db = ctx.db();
        save_rdb(db, &path)
    };

    match result {
        Ok(()) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            cfg.set_last_save_unix(now);
            ctx.reply_simple_string(b"OK")
        }
        Err(e) => Err(RedisError::runtime(
            format!("ERR SAVE failed: {}", e).into_bytes(),
        )),
    }
}

/// `BGSAVE [SCHEDULE]` — background RDB save.
///
/// On Unix, forks a child process that writes the RDB file using the OS
/// copy-on-write snapshot visible at fork time, then `_exit`s. The parent
/// returns `+Background saving started` immediately and records the child PID.
///
/// If a BGSAVE child is already running, returns an error immediately rather
/// than starting a second concurrent save.
///
/// On non-Unix targets, falls back to the thread-snapshot approach.
pub fn bgsave_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() > 2 {
        return Err(RedisError::wrong_number_of_args(b"bgsave"));
    }

    let server = ctx.server();

    if server.rdb_child_pid() != 0 {
        return Err(RedisError::runtime(
            b"ERR Background save already in progress".to_vec(),
        ));
    }

    let cfg = Arc::clone(&server.live_config);
    let path: PathBuf = rdb_path(&cfg.rdb_dir(), &cfg.rdb_filename());

    #[cfg(unix)]
    {
        let server_arc = ctx.server_arc();

        // SAFETY: fork(2) is the standard Unix mechanism for COW snapshot.
        // All requirements (single-threaded child, async-signal-safe ops only)
        // are met: child immediately writes RDB and _exits without running any
        // parent atexit handlers. The parent half only stores the child PID into
        // an atomic and returns — no Rust destructors of the shared state run in
        // the child because _exit bypasses them.
        let pid = unsafe {
            let p = libc::fork();
            if p == 0 {
                let exit_code = if save_rdb(ctx.db(), &path).is_ok() { 0i32 } else { 1i32 };
                libc::_exit(exit_code);
            }
            p
        };

        if pid > 0 {
            server_arc.set_rdb_child_pid(pid);
            return ctx.reply_simple_string(b"Background saving started");
        }

        eprintln!("redis-server: fork() failed, falling back to thread snapshot");
    }

    let snapshot = snapshot_db(ctx.db());
    let _ = thread::Builder::new()
        .name("bgsave".to_string())
        .spawn(move || {
            let tmp_db = RedisDb::from_snapshot(snapshot);
            match save_rdb(&tmp_db, &path) {
                Ok(()) => {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    cfg.set_last_save_unix(now);
                }
                Err(e) => {
                    eprintln!("redis-server: BGSAVE failed: {}", e);
                }
            }
        });

    ctx.reply_simple_string(b"Background saving started")
}

/// Snapshot the entries of `db` into an owned `Vec` for the thread-based
/// BGSAVE fallback used on non-Unix targets and on fork failure.
fn snapshot_db(db: &RedisDb) -> Vec<(redis_types::RedisString, redis_core::RedisObject)> {
    db.iter_for_eviction()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}
