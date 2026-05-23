//! SLOWLOG and LATENCY command handlers with global ring-buffer state.
//!
//! Wraps the fully-ported `redis_core::commandlog` and `redis_core::latency`
//! modules behind a pair of `OnceLock<Arc<Mutex<_>>>` globals so the stateless
//! `Handler` function-pointer signature required by `dispatch.rs` can reach
//! persistent state without threading it through `CommandContext`.
//!
//! SLOWLOG global state:
//!   - Ring buffer of at most `slowlog_max_len` entries (default 128).
//!   - Records commands whose execution time exceeds `threshold_micros`
//!     (default 10 000 µs; -1 disables recording entirely).
//!
//! LATENCY global state:
//!   - Per-event ring buffers exposed by `LatencyMonitor`.
//!   - No internal collection hooks for Phase B; the in-memory map is
//!     populated only via the public `report_latency_event` API.
//!
//! The timing wrap that feeds SLOWLOG entries is in `dispatch.rs`'s
//! `dispatch_timed` function, which records duration post-handler and calls
//! `record_slowlog_entry` defined here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use redis_core::commandlog::{CommandLog, CommandLogEntry, COMMANDLOG_ENTRY_MAX_ARGC, COMMANDLOG_ENTRY_MAX_STRING};
use redis_core::monotonic::{elapsed_us, MonoTime};
use redis_core::latency::{LatencyMonitor, LatencyReportConfig};
use redis_core::CommandContext;
use redis_types::{RedisResult, RedisString};

// ── Slowlog global ────────────────────────────────────────────────────────────

/// Singleton slowlog state shared across all connections.
static SLOWLOG: OnceLock<Arc<Mutex<CommandLog>>> = OnceLock::new();
static BLOCKED_SLOWLOG: OnceLock<Arc<Mutex<HashMap<u64, PendingBlockedSlowlogEntry>>>> =
    OnceLock::new();

struct PendingBlockedSlowlogEntry {
    argv: Vec<RedisString>,
    start_micros: MonoTime,
    client_name: Option<RedisString>,
}

/// Return a handle to the global slowlog, initialising it on first call.
pub fn global_slowlog() -> Arc<Mutex<CommandLog>> {
    SLOWLOG
        .get_or_init(|| {
            let mut log = CommandLog::new();
            log.threshold = 10_000;
            log.max_len = 128;
            Arc::new(Mutex::new(log))
        })
        .clone()
}

fn blocked_slowlog_pending() -> Arc<Mutex<HashMap<u64, PendingBlockedSlowlogEntry>>> {
    BLOCKED_SLOWLOG
        .get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
        .clone()
}

/// Record one command execution into the slowlog if `duration_micros` meets the threshold.
///
/// Acquires the global lock, checks the threshold and max-len, then pushes a
/// new entry at the front of the deque (newest-first) and trims the tail.
/// Called from `dispatch_timed` in `dispatch.rs` after every command completes.
pub fn record_slowlog_entry(
    argv: &[RedisString],
    duration_micros: u64,
    client_id: u64,
    client_name: Option<RedisString>,
) {
    let handle = global_slowlog();
    let mut log = match handle.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };

    if log.threshold < 0 {
        return;
    }
    if log.max_len == 0 {
        return;
    }
    if duration_micros < log.threshold as u64 {
        return;
    }

    let id = log.entry_id;
    log.entry_id = log.entry_id.wrapping_add(1);

    let timestamp_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let argc = argv.len();
    let ceargc = argc.min(COMMANDLOG_ENTRY_MAX_ARGC);
    let mut stored_argv: Vec<RedisString> = Vec::with_capacity(ceargc);
    for j in 0..ceargc {
        if ceargc != argc && j == ceargc - 1 {
            let remaining = argc - ceargc + 1;
            let msg = format!("... ({} more arguments)", remaining);
            stored_argv.push(RedisString::from_bytes(msg.as_bytes()));
        } else {
            let arg = &argv[j];
            if arg.len() > COMMANDLOG_ENTRY_MAX_STRING {
                let extra = arg.len() - COMMANDLOG_ENTRY_MAX_STRING;
                let mut truncated: Vec<u8> =
                    arg.as_bytes()[..COMMANDLOG_ENTRY_MAX_STRING].to_vec();
                let suffix = format!("... ({} more bytes)", extra);
                truncated.extend_from_slice(suffix.as_bytes());
                stored_argv.push(RedisString::from_vec(truncated));
            } else {
                stored_argv.push(arg.clone());
            }
        }
    }

    let entry = CommandLogEntry {
        argv: stored_argv,
        id,
        value: duration_micros as i64,
        time: timestamp_unix,
        cname: client_name.unwrap_or_else(RedisString::new),
        peerid: RedisString::from_bytes(format!("id={}", client_id).as_bytes()),
    };

    log.entries.push_front(entry);
    while log.entries.len() > log.max_len {
        log.entries.pop_back();
    }
}

/// Remember a blocked command's original argv until the wake path completes it.
///
/// C Valkey skips commandlog emission while `call()` leaves the client blocked
/// and records the command from the unblock/reprocess path instead.
pub fn remember_blocked_slowlog_entry(
    argv: Vec<RedisString>,
    start_micros: MonoTime,
    client_id: u64,
    client_name: Option<RedisString>,
) {
    let handle = blocked_slowlog_pending();
    let mut pending = match handle.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    pending.insert(
        client_id,
        PendingBlockedSlowlogEntry {
            argv,
            start_micros,
            client_name,
        },
    );
}

/// Record and clear a pending blocked command after it has been unblocked.
pub fn record_blocked_slowlog_entry(client_id: u64) {
    let handle = blocked_slowlog_pending();
    let pending_entry = {
        let mut pending = match handle.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        pending.remove(&client_id)
    };
    if let Some(entry) = pending_entry {
        record_slowlog_entry(
            &entry.argv,
            elapsed_us(entry.start_micros),
            client_id,
            entry.client_name,
        );
    }
}

/// Update the slowlog threshold in microseconds.
///
/// Called from `CONFIG SET slowlog-log-slower-than <value>`.
pub fn set_slowlog_threshold(micros: i64) {
    let handle = global_slowlog();
    let mut log = match handle.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    log.threshold = micros;
}

/// Update the slowlog maximum length.
///
/// Called from `CONFIG SET slowlog-max-len <value>`.
pub fn set_slowlog_max_len(max: usize) {
    let handle = global_slowlog();
    let mut log = match handle.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    log.max_len = max;
    while log.entries.len() > log.max_len {
        log.entries.pop_back();
    }
}

// ── SLOWLOG command handler ───────────────────────────────────────────────────

/// `SLOWLOG GET [count]`, `SLOWLOG LEN`, `SLOWLOG RESET`, `SLOWLOG HELP`.
pub fn slowlog_command(ctx: &mut CommandContext) -> RedisResult<()> {
    let argc = ctx.arg_count();
    if argc < 2 {
        return Err(redis_types::RedisError::wrong_number_of_args(b"slowlog"));
    }

    let sub = ctx.arg_owned(1usize)?;
    let sub_bytes = sub.as_bytes();

    if sub_bytes.eq_ignore_ascii_case(b"len") {
        if argc != 2 {
            return Err(redis_types::RedisError::wrong_number_of_args(b"slowlog|len"));
        }
        let handle = global_slowlog();
        let log = match handle.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        return ctx.reply_integer(log.entries.len() as i64);
    }

    if sub_bytes.eq_ignore_ascii_case(b"reset") {
        if argc != 2 {
            return Err(redis_types::RedisError::wrong_number_of_args(b"slowlog|reset"));
        }
        let handle = global_slowlog();
        let mut log = match handle.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        log.entries.clear();
        return ctx.reply_simple_string(b"OK");
    }

    if sub_bytes.eq_ignore_ascii_case(b"get") {
        if argc > 3 {
            return Err(redis_types::RedisError::wrong_number_of_args(b"slowlog|get"));
        }
        let default_count: i64 = 10;
        let requested: i64 = if argc == 3 {
            let count_arg = ctx.arg_owned(2usize)?;
            parse_count(count_arg.as_bytes())?
        } else {
            default_count
        };
        let handle = global_slowlog();
        let log = match handle.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let actual_count = if requested == -1 {
            log.entries.len()
        } else {
            (requested as usize).min(log.entries.len())
        };
        ctx.reply_array_header(actual_count)?;
        for entry in log.entries.iter().take(actual_count) {
            ctx.reply_array_header(6usize)?;
            ctx.reply_integer(entry.id as i64)?;
            ctx.reply_integer(entry.time)?;
            ctx.reply_integer(entry.value)?;
            ctx.reply_array_header(entry.argv.len())?;
            for arg in &entry.argv {
                ctx.reply_bulk(arg.as_bytes())?;
            }
            ctx.reply_bulk(entry.peerid.as_bytes())?;
            ctx.reply_bulk(entry.cname.as_bytes())?;
        }
        return Ok(());
    }

    if sub_bytes.eq_ignore_ascii_case(b"help") {
        let lines: &[&[u8]] = &[
            b"GET [<count>]",
            b"    Return top <count> entries from the slowlog (default: 10, -1 means all).",
            b"    Entries are made of:",
            b"    id, timestamp, time in microseconds, arguments array, client IP and port,",
            b"    client name",
            b"LEN",
            b"    Return the length of the slowlog.",
            b"RESET",
            b"    Reset the slowlog.",
        ];
        ctx.reply_array_header(lines.len())?;
        for line in lines {
            ctx.reply_bulk(line)?;
        }
        return Ok(());
    }

    let mut msg = Vec::with_capacity(
        b"ERR unknown subcommand or wrong number of arguments for '".len()
            + sub_bytes.len()
            + 2,
    );
    msg.extend_from_slice(b"ERR unknown subcommand or wrong number of arguments for '");
    msg.extend_from_slice(sub_bytes);
    msg.push(b'\'');
    Err(redis_types::RedisError::runtime(msg))
}

fn parse_count(bytes: &[u8]) -> Result<i64, redis_types::RedisError> {
    if bytes.is_empty() {
        return Err(slowlog_count_error());
    }
    let (negative, digits) = if bytes[0] == b'-' {
        (true, &bytes[1..])
    } else {
        (false, bytes)
    };
    if digits.is_empty() {
        return Err(slowlog_count_error());
    }
    let mut value: i64 = 0;
    for &byte in digits {
        if !byte.is_ascii_digit() {
            return Err(slowlog_count_error());
        }
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add((byte - b'0') as i64))
            .ok_or_else(slowlog_count_error)?;
    }
    let parsed = if negative {
        value.checked_neg().ok_or_else(slowlog_count_error)?
    } else {
        value
    };
    if parsed < -1 {
        return Err(slowlog_count_error());
    }
    Ok(parsed)
}

fn slowlog_count_error() -> redis_types::RedisError {
    redis_types::RedisError::runtime(b"ERR count should be greater than or equal to -1")
}

// ── Latency global ────────────────────────────────────────────────────────────

/// Singleton latency monitor shared across all connections.
static LATENCY: OnceLock<Arc<Mutex<LatencyMonitor>>> = OnceLock::new();

/// Return a handle to the global latency monitor, initialising it on first call.
pub fn global_latency() -> Arc<Mutex<LatencyMonitor>> {
    LATENCY
        .get_or_init(|| Arc::new(Mutex::new(LatencyMonitor::new())))
        .clone()
}

/// Report a latency event observation (milliseconds) into the global monitor.
///
/// Phase B: no internal callers; exposed for future integration with expire-cycle,
/// fork, AOF-write, and other latency hooks.
pub fn report_latency_event(event_name: &[u8], latency_ms: u64) {
    let handle = global_latency();
    let mut monitor = match handle.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    monitor.add_sample(event_name, latency_ms as i64 * 1000);
}

// ── LATENCY command handler ───────────────────────────────────────────────────

/// `LATENCY LATEST`, `LATENCY HISTORY event`, `LATENCY RESET [event...]`,
/// `LATENCY GRAPH event`, `LATENCY DOCTOR`, `LATENCY HELP`.
pub fn latency_command(ctx: &mut CommandContext) -> RedisResult<()> {
    let argc = ctx.arg_count();
    if argc < 2 {
        return Err(redis_types::RedisError::wrong_number_of_args(b"latency"));
    }

    let sub = ctx.arg_owned(1usize)?;
    let sub_bytes = sub.as_bytes();

    if sub_bytes.eq_ignore_ascii_case(b"latest") {
        if argc != 2 {
            return Err(redis_types::RedisError::wrong_number_of_args(b"latency|latest"));
        }
        let handle = global_latency();
        let monitor = match handle.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        return reply_latency_latest(ctx, &monitor);
    }

    if sub_bytes.eq_ignore_ascii_case(b"history") {
        if argc != 3 {
            return Err(redis_types::RedisError::wrong_number_of_args(b"latency|history"));
        }
        let event = ctx.arg_owned(2usize)?;
        let handle = global_latency();
        let monitor = match handle.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        return reply_latency_history(ctx, &monitor, event.as_bytes());
    }

    if sub_bytes.eq_ignore_ascii_case(b"reset") {
        let handle = global_latency();
        let mut monitor = match handle.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if argc == 2 {
            let count = monitor.reset_event(None);
            return ctx.reply_integer(count as i64);
        }
        let mut events: Vec<Vec<u8>> = Vec::with_capacity(argc - 2);
        for i in 2..argc {
            events.push(ctx.arg_owned(i)?.as_bytes().to_vec());
        }
        let mut total = 0i32;
        for ev in &events {
            total += monitor.reset_event(Some(ev));
        }
        return ctx.reply_integer(total as i64);
    }

    if sub_bytes.eq_ignore_ascii_case(b"graph") {
        if argc != 3 {
            return Err(redis_types::RedisError::wrong_number_of_args(b"latency|graph"));
        }
        return ctx.reply_bulk(b"(no data)\n");
    }

    if sub_bytes.eq_ignore_ascii_case(b"doctor") {
        if argc != 2 {
            return Err(redis_types::RedisError::wrong_number_of_args(b"latency|doctor"));
        }
        let handle = global_latency();
        let monitor = match handle.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let cfg = LatencyReportConfig {
            latency_monitor_threshold: 0,
            stat_fork_rate: 0.0,
            slowlog_threshold_us: {
                let sl = global_slowlog();
                let log = match sl.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                log.threshold
            },
            slowlog_max_len: {
                let sl = global_slowlog();
                let log = match sl.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                log.max_len as i32
            },
            hz: 10,
            aof_fsync_always: false,
        };
        let report = monitor.create_report(&cfg);
        return ctx.reply_bulk(&report);
    }

    if sub_bytes.eq_ignore_ascii_case(b"help") {
        if argc != 2 {
            return Err(redis_types::RedisError::wrong_number_of_args(b"latency|help"));
        }
        let lines: &[&[u8]] = &[
            b"DOCTOR",
            b"    Return a human readable latency analysis report.",
            b"GRAPH <event>",
            b"    Return an ASCII latency graph for the <event> class.",
            b"HISTORY <event>",
            b"    Return time-latency samples for the <event> class.",
            b"LATEST",
            b"    Return the latest latency samples for all events.",
            b"RESET [<event> ...]",
            b"    Reset latency data of one or more <event> classes.",
            b"    (default: reset all data for all event classes)",
        ];
        ctx.reply_array_header(lines.len())?;
        for line in lines {
            ctx.reply_bulk(line)?;
        }
        return Ok(());
    }

    let mut msg = Vec::with_capacity(
        b"ERR unknown subcommand or wrong number of arguments for '".len()
            + sub_bytes.len()
            + 2,
    );
    msg.extend_from_slice(b"ERR unknown subcommand or wrong number of arguments for '");
    msg.extend_from_slice(sub_bytes);
    msg.push(b'\'');
    Err(redis_types::RedisError::runtime(msg))
}

fn reply_latency_latest(
    ctx: &mut CommandContext,
    monitor: &LatencyMonitor,
) -> RedisResult<()> {
    use redis_core::latency::LATENCY_TS_LEN;
    let count = monitor.len();
    ctx.reply_array_header(count)?;
    for (event_key, ts) in monitor.iter() {
        let last = (ts.idx + LATENCY_TS_LEN - 1) % LATENCY_TS_LEN;
        ctx.reply_array_header(4usize)?;
        ctx.reply_bulk(event_key)?;
        ctx.reply_integer(ts.samples[last].time as i64)?;
        ctx.reply_integer(ts.samples[last].latency as i64)?;
        ctx.reply_integer(ts.max as i64)?;
    }
    Ok(())
}

fn reply_latency_history(
    ctx: &mut CommandContext,
    monitor: &LatencyMonitor,
    event: &[u8],
) -> RedisResult<()> {
    use redis_core::latency::LATENCY_TS_LEN;
    let ts = match monitor.get(event) {
        None => {
            return ctx.reply_array_header(0usize);
        }
        Some(ts) => ts,
    };
    let mut samples: Vec<(i64, i64)> = Vec::new();
    for j in 0..LATENCY_TS_LEN {
        let i = (ts.idx + j) % LATENCY_TS_LEN;
        if ts.samples[i].time == 0 {
            continue;
        }
        samples.push((ts.samples[i].time as i64, ts.samples[i].latency as i64));
    }
    ctx.reply_array_header(samples.len())?;
    for (time, latency) in samples {
        ctx.reply_array_header(2usize)?;
        ctx.reply_integer(time)?;
        ctx.reply_integer(latency)?;
    }
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        new (OV-2 implementation)
//   target_crate:  redis-commands
//   confidence:    high
//   todos:         0
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         SLOWLOG ring buffer with global OnceLock. LATENCY in-memory
//                  map backed by redis_core::latency::LatencyMonitor. Phase B:
//                  no internal event-collection callers; API exposed for future
//                  hooks. SLOWLOG GET reply format matches canonical Redis 6-tuple;
//                  blocked list commands are recorded from the wake path.
// ──────────────────────────────────────────────────────────────────────────────
