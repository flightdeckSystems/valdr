//! Runtime transport: the live `Connection` enum used by the redis-server binary.
//!
//! Wave A pilot abstraction. Owns a real OS handle (currently only `TcpStream`)
//! and presents a small read/write/close API to the event loop in
//! `redis-server::main`.
//!
//! # Why this is separate from `connection.rs`
//!
//! `connection.rs` is the C-faithful port of `connection.c`/`connection.h`
//! (vtable-based registry, `ConnectionTypeTrait`, file descriptors). It is
//! intended to eventually back this module's variants but is not yet wired
//! to a real backend (the registry has no implementations registered).
//!
//! The Wave A pilot needs a synchronous, working TCP transport *now* so the
//! binary can accept connections. This module provides that minimal surface
//! and is the type referenced by `Client::with_connection`.
//!
//! TODO(architect): collapse this with `connection.rs` once concrete
//! `ConnectionTypeTrait` backends (socket / unix / tls) land in Phase 5+.
//! Until then the two coexist: `connection::Connection` is the C-ported
//! struct with `fd: i32`; `transport::Connection` is the live runtime enum.

use std::io;
use std::net::{SocketAddr, TcpStream};

/// Live connection used by the running redis-server binary.
///
/// Variants are added as backends land. The Wave A pilot only ships `Tcp`;
/// `Unix` and `Tls` arrive in Phase 5+.
pub enum Connection {
    /// Plain blocking TCP connection.
    Tcp(TcpStream),
    // TODO(architect): Unix, Tls variants — Phase 5+ when those backends land.
}

impl Connection {
    /// Read up to `buf.len()` bytes from the connection.
    ///
    /// Returns the number of bytes read; `0` indicates EOF.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use io::Read;
        match self {
            Connection::Tcp(s) => s.read(buf),
        }
    }

    /// Write the entire buffer to the connection, retrying on short writes.
    pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        use io::Write;
        match self {
            Connection::Tcp(s) => s.write_all(buf),
        }
    }

    /// Close the connection by dropping the underlying handle.
    ///
    /// Equivalent to `std::mem::drop(self)`; provided so call sites can be
    /// explicit about intent.
    pub fn close(self) {
        drop(self);
    }

    /// Return the peer address (remote end) of the connection.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Connection::Tcp(s) => s.peer_addr(),
        }
    }
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Connection::Tcp(s) => match s.peer_addr() {
                Ok(addr) => write!(f, "Connection::Tcp({})", addr),
                Err(_) => write!(f, "Connection::Tcp(<closed>)"),
            },
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        architect packet (Wave A — runtime TCP transport)
//   target_crate:  redis-core
//   confidence:    high
//   todos:         2
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         Minimal blocking-I/O Connection enum. Wraps TcpStream only;
//                  Unix/Tls deferred. Coexists with the C-ported connection.rs
//                  vtable module until backends are unified in Phase 5+.
// ──────────────────────────────────────────────────────────────────────────
