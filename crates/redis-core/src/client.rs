//! `Client` — per-connection state.
//!
//! Minimal scaffolding for the pilot. Holds parsed-command args and
//! pending reply bytes. No event-loop integration or connection
//! abstraction yet — those land in Phase 2-3 with the architect deciding
//! sync/async strategy after we measure.

use redis_protocol::RespFrame;
use redis_types::RedisString;

pub type ClientId = u64;

#[derive(Debug)]
pub struct Client {
    /// Server-assigned client identifier (CLIENT ID).
    pub id: ClientId,
    /// Parsed args of the current command (cleared per command).
    pub argv: Vec<RedisString>,
    /// Pending reply bytes, drained by the I/O layer.
    pub reply_buf: Vec<u8>,
    /// Selected database index (Phase 3 with RedisDb).
    pub db_index: u32,
}

impl Client {
    pub fn new(id: ClientId) -> Self {
        Self {
            id,
            argv: Vec::new(),
            reply_buf: Vec::new(),
            db_index: 0,
        }
    }

    pub fn arg(&self, i: usize) -> Option<&RedisString> {
        self.argv.get(i)
    }

    pub fn arg_count(&self) -> usize {
        self.argv.len()
    }

    pub fn reset_args(&mut self) {
        self.argv.clear();
    }

    pub fn set_args(&mut self, args: Vec<RedisString>) {
        self.argv = args;
    }

    /// Append an encoded RESP frame to the pending-reply buffer.
    pub fn write_frame(&mut self, frame: &RespFrame) {
        redis_protocol::encode_resp2(frame, &mut self.reply_buf);
    }

    /// Drain the reply buffer; caller (I/O layer) writes to the socket.
    pub fn drain_reply(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.reply_buf)
    }

    /// `process_input` parses raw bytes from the socket into commands.
    /// Translation packet for `networking.c::processInputBuffer` fills this.
    pub fn process_input(&mut self, _bytes: &[u8]) -> redis_types::RedisResult<()> {
        // TODO(port): port networking.c::processInputBuffer here.
        todo!("port networking.c::processInputBuffer in Phase 2")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_frame_appends_to_reply_buf() {
        let mut c = Client::new(1);
        c.write_frame(&RespFrame::simple(b"OK".as_slice()));
        c.write_frame(&RespFrame::integer(42));
        let bytes = c.drain_reply();
        assert_eq!(bytes, b"+OK\r\n:42\r\n");
        assert!(c.drain_reply().is_empty());
    }

    #[test]
    fn args_access() {
        let mut c = Client::new(2);
        c.set_args(vec![
            RedisString::from_bytes(b"SET"),
            RedisString::from_bytes(b"foo"),
            RedisString::from_bytes(b"bar"),
        ]);
        assert_eq!(c.arg_count(), 3);
        assert_eq!(c.arg(0).unwrap().as_bytes(), b"SET");
        assert_eq!(c.arg(99), None);
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        architect packet (PORTING.md §2 #5 + types.tsv:client mapping)
//   target_crate:  redis-core
//   confidence:    high
//   todos:         1
//   port_notes:    1
//   unsafe_blocks: 0
//   notes:         Minimal Client; process_input is todo!() until networking.c is ported.
// ──────────────────────────────────────────────────────────────────────────
