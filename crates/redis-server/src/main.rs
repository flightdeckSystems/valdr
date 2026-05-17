//! `redis-server` binary entry point — Wave A scaffolding.
//!
//! Minimal TCP accept loop: binds a port, spawns a thread per accepted
//! connection, reads RESP requests, dispatches through `redis-commands`,
//! and writes the reply back to the socket.
//!
//! Out of scope for Wave A:
//!   * Event-loop based I/O (no `mio` / `tokio`); blocking thread-per-conn.
//!   * Multi-DB routing (every command sees DB 0).
//!   * Real command bodies — Waves B/C/D fill those in.
//!   * Replication, cluster, persistence, pub/sub, modules.

use std::io;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use redis_commands::dispatch;
use redis_core::{Client, Connection};
use redis_core::command_context::CommandContext;
use redis_core::db::RedisDb;
use redis_protocol::parse_inline_or_multibulk;
use redis_protocol::frame::{RespFrame, encode_resp2};
use redis_types::{RedisError, RedisString};

const DEFAULT_PORT: u16 = 6379;
const DEFAULT_BIND: &str = "127.0.0.1";

/// Parsed command-line arguments.
struct CliArgs {
    port: u16,
    bind: String,
}

impl Default for CliArgs {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            bind: DEFAULT_BIND.to_string(),
        }
    }
}

/// Parse the supported `--port <N>` and `--bind <addr>` flags from CLI args.
///
/// Unrecognised flags are reported to stderr but do not abort startup; that
/// matches the Wave A "minimal CLI" goal. Phase 3+ will wire the full
/// `config.c` argument parser.
fn parse_args(argv: Vec<String>) -> Result<CliArgs, String> {
    let mut out = CliArgs::default();
    let mut it = argv.into_iter().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--port" | "-p" => {
                let v = it.next().ok_or_else(|| "--port requires a value".to_string())?;
                out.port = v.parse().map_err(|_| format!("invalid port: {}", v))?;
            }
            "--bind" => {
                let v = it.next().ok_or_else(|| "--bind requires a value".to_string())?;
                out.bind = v;
            }
            "--help" | "-h" => {
                println!("Usage: redis-server [--port N] [--bind addr]");
                std::process::exit(0);
            }
            other => {
                eprintln!("redis-server: ignoring unknown flag '{}'", other);
            }
        }
    }
    Ok(out)
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let args = match parse_args(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("redis-server: {}", e);
            std::process::exit(1);
        }
    };

    let bind_ip: IpAddr = match args.bind.parse() {
        Ok(ip) => ip,
        Err(_) => {
            eprintln!(
                "redis-server: --bind expects an IP literal (got '{}'); hostnames not yet supported",
                args.bind
            );
            std::process::exit(1);
        }
    };
    let addr = SocketAddr::new(bind_ip, args.port);

    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("redis-server: bind {} failed: {}", addr, e);
            std::process::exit(1);
        }
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    install_shutdown_handler(Arc::clone(&shutdown));

    if let Err(e) = listener.set_nonblocking(false) {
        eprintln!("redis-server: set_nonblocking(false) failed: {}", e);
    }
    eprintln!("redis-server: listening on {}", addr);

    let db = Arc::new(Mutex::new(RedisDb::new(0)));
    let next_client_id = Arc::new(AtomicU64::new(1));
    serve(listener, shutdown, db, next_client_id);
}

/// Best-effort SIGINT/SIGTERM handler. Without any signal-handling deps we
/// install nothing and rely on the OS to terminate the process on Ctrl-C.
///
/// TODO(architect): wire a proper graceful-shutdown handler (signal-hook,
/// ctrlc, or a hand-rolled `sigaction` via libc) in Phase 3.
fn install_shutdown_handler(_shutdown: Arc<AtomicBool>) {
    // No-op — the AtomicBool exists so the accept loop can be wired to it
    // once a signal-handling dependency lands.
}

/// Accept loop. One std::thread per accepted connection.
fn serve(
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    db: Arc<Mutex<RedisDb>>,
    next_client_id: Arc<AtomicU64>,
) {
    for incoming in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            eprintln!("redis-server: shutdown requested, exiting accept loop");
            return;
        }
        match incoming {
            Ok(stream) => {
                if let Err(e) = stream.set_nodelay(true) {
                    eprintln!("redis-server: set_nodelay failed: {}", e);
                }
                let peer = stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| "<unknown>".to_string());
                let shutdown = Arc::clone(&shutdown);
                let db = Arc::clone(&db);
                let id = next_client_id.fetch_add(1, Ordering::Relaxed);
                let _ = thread::Builder::new()
                    .name(format!("client-{}", peer))
                    .spawn(move || handle_connection(stream, shutdown, db, id, peer));
            }
            Err(e) => {
                eprintln!("redis-server: accept failed: {}", e);
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
            }
        }
    }
}

/// Per-connection event loop. Reads from the socket, feeds the incremental
/// parser, dispatches each completed command, writes the reply.
fn handle_connection(
    stream: TcpStream,
    shutdown: Arc<AtomicBool>,
    db: Arc<Mutex<RedisDb>>,
    id: u64,
    peer_addr: String,
) {
    let mut client = Client::with_connection(Connection::Tcp(stream));
    client.id = id;
    client.addr = Some(peer_addr);
    let mut read_buf = [0u8; 16 * 1024];

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }

        let conn = match client.conn.as_mut() {
            Some(c) => c,
            None => return,
        };

        let n = match conn.read(&mut read_buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };

        client.query_buf.extend_from_slice(&read_buf[..n]);

        loop {
            let parsed = parse_inline_or_multibulk(&client.query_buf);
            match parsed {
                Ok(Some((argv, consumed))) => {
                    client.query_buf.drain(..consumed);
                    process_command(&mut client, argv, &db);
                }
                Ok(None) => break,
                Err(err) => {
                    write_error_and_disconnect(&mut client, &err);
                    return;
                }
            }

            if !flush_reply(&mut client) {
                return;
            }

            if client.should_close {
                return;
            }
        }

        if !flush_reply(&mut client) {
            return;
        }

        if client.should_close {
            return;
        }
    }
}

/// Install `argv` as the current command and route through the dispatcher.
///
/// On error, the error is written to the reply buffer as a RESP `-...` line so
/// the I/O layer can flush it like any other reply.
fn process_command(client: &mut Client, argv: Vec<RedisString>, db: &Arc<Mutex<RedisDb>>) {
    client.set_args(argv);
    let result = {
        let mut guard = match db.lock() {
            Ok(g) => g,
            Err(poison) => poison.into_inner(),
        };
        let mut ctx = CommandContext::with_db(client, &mut guard);
        dispatch(&mut ctx)
    };
    if let Err(err) = result {
        let payload = err.to_resp_payload();
        encode_resp2(&RespFrame::Error(payload), &mut client.reply_buf);
    }
    client.reset_args();
}

/// Drain `client.reply_buf` to the socket. Returns `false` if the connection
/// should be torn down (write failure).
fn flush_reply(client: &mut Client) -> bool {
    if client.reply_buf.is_empty() {
        return true;
    }
    let conn = match client.conn.as_mut() {
        Some(c) => c,
        None => return false,
    };
    let bytes = std::mem::take(&mut client.reply_buf);
    conn.write_all(&bytes).is_ok()
}

/// Encode `err` as a RESP error and try to flush it before dropping the
/// connection. Used on fatal protocol errors.
fn write_error_and_disconnect(client: &mut Client, err: &RedisError) {
    let payload = err.to_resp_payload();
    encode_resp2(&RespFrame::Error(payload), &mut client.reply_buf);
    let _ = flush_reply(client);
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        architect packet (Wave A — main entry + accept loop)
//   target_crate:  redis-server
//   confidence:    high
//   todos:         1
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         Blocking thread-per-conn TCP server. SIGINT handler is a
//                  no-op stub (no ctrlc/signal-hook dep yet). Dispatch routes
//                  every command through redis-commands::dispatch; unknown +
//                  unimplemented commands return clean RESP error replies.
// ──────────────────────────────────────────────────────────────────────────
