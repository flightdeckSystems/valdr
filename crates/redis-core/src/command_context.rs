//! `CommandContext` — the contract every command implementation works against.
//!
//! Per PORTING.md §2 #5: bundles `&mut Client`, parsed args, and reply
//! writer helpers. Returns `Result<(), RedisError>`. NOT the C `client *c`
//! parameter — commands never touch the raw connection or buffer-list.
//!
//! `RedisServer` reference comes via the orchestrator (Phase 3 architect
//! packet adds it).

use crate::client::Client;
use redis_protocol::RespFrame;
use redis_types::{RedisError, RedisResult, RedisString};

/// Bundle of context every command receives. Wraps a mutable Client and
/// exposes argument access + reply-writer methods.
pub struct CommandContext<'a> {
    pub client: &'a mut Client,
    // TODO(architect): Phase 3 — add `&mut RedisServer` here, then
    // commands can reach global state (config, db list, replication, etc.).
}

impl<'a> CommandContext<'a> {
    pub fn new(client: &'a mut Client) -> Self {
        Self { client }
    }

    // ── Args ──────────────────────────────────────────────────────

    pub fn arg(&self, i: usize) -> RedisResult<&RedisString> {
        self.client
            .arg(i)
            .ok_or_else(|| RedisError::wrong_number_of_args(self.command_name()))
    }

    pub fn arg_count(&self) -> usize {
        self.client.arg_count()
    }

    /// Arg 0 is the command name (uppercase by Redis convention).
    pub fn command_name(&self) -> &[u8] {
        self.client
            .arg(0)
            .map(|s| s.as_bytes())
            .unwrap_or(b"<unknown>")
    }

    // ── Reply writers ─────────────────────────────────────────────

    pub fn reply_simple_string(&mut self, bytes: &[u8]) -> RedisResult<()> {
        self.client
            .write_frame(&RespFrame::Simple(RedisString::from_bytes(bytes)));
        Ok(())
    }

    pub fn reply_bulk(&mut self, bytes: &[u8]) -> RedisResult<()> {
        self.client
            .write_frame(&RespFrame::Bulk(Some(RedisString::from_bytes(bytes))));
        Ok(())
    }

    pub fn reply_bulk_string(&mut self, s: RedisString) -> RedisResult<()> {
        self.client.write_frame(&RespFrame::Bulk(Some(s)));
        Ok(())
    }

    pub fn reply_null_bulk(&mut self) -> RedisResult<()> {
        self.client.write_frame(&RespFrame::Bulk(None));
        Ok(())
    }

    pub fn reply_integer(&mut self, n: i64) -> RedisResult<()> {
        self.client.write_frame(&RespFrame::Integer(n));
        Ok(())
    }

    pub fn reply_array_header(&mut self, len: usize) -> RedisResult<()> {
        // Architect note: streaming-array reply is the common case; full
        // RespFrame::Array(Some(vec![...])) is for when the array is
        // already materialized. Header-then-elements is the equivalent of
        // C's addReplyArrayLen + element-by-element addReply* calls.
        let mut buf = Vec::new();
        buf.push(b'*');
        use std::io::Write;
        let _ = write!(buf, "{}", len);
        buf.extend_from_slice(b"\r\n");
        self.client.reply_buf.extend_from_slice(&buf);
        Ok(())
    }

    pub fn reply_null_array(&mut self) -> RedisResult<()> {
        self.client.write_frame(&RespFrame::Array(None));
        Ok(())
    }

    pub fn reply_frame(&mut self, frame: &RespFrame) -> RedisResult<()> {
        self.client.write_frame(frame);
        Ok(())
    }

    /// Reply with a RedisError. Equivalent of C's addReplyError* family.
    /// The error becomes a `-...` RESP error line; doesn't return Err.
    pub fn reply_error(&mut self, err: &RedisError) -> RedisResult<()> {
        self.client
            .write_frame(&RespFrame::Error(err.to_resp_payload()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_args(args: &[&[u8]]) -> (Client, ) {
        let mut c = Client::new(1);
        c.set_args(args.iter().map(|s| RedisString::from_bytes(s)).collect());
        (c,)
    }

    #[test]
    fn arg_access_returns_err_when_oob() {
        let (mut c,) = ctx_with_args(&[b"SET", b"foo"]);
        let ctx = CommandContext::new(&mut c);
        assert!(ctx.arg(0).is_ok());
        assert!(ctx.arg(1).is_ok());
        let err = ctx.arg(2).unwrap_err();
        assert!(matches!(err, RedisError::WrongNumberOfArgs(_)));
    }

    #[test]
    fn reply_simple_string_writes_resp() {
        let (mut c,) = ctx_with_args(&[b"PING"]);
        let mut ctx = CommandContext::new(&mut c);
        ctx.reply_simple_string(b"PONG").unwrap();
        assert_eq!(c.drain_reply(), b"+PONG\r\n");
    }

    #[test]
    fn reply_array_header_emits_correct_prefix() {
        let (mut c,) = ctx_with_args(&[]);
        let mut ctx = CommandContext::new(&mut c);
        ctx.reply_array_header(3).unwrap();
        ctx.reply_integer(1).unwrap();
        ctx.reply_integer(2).unwrap();
        ctx.reply_integer(3).unwrap();
        assert_eq!(c.drain_reply(), b"*3\r\n:1\r\n:2\r\n:3\r\n");
    }

    #[test]
    fn reply_error_emits_error_line() {
        let (mut c,) = ctx_with_args(&[]);
        let mut ctx = CommandContext::new(&mut c);
        ctx.reply_error(&RedisError::wrong_type()).unwrap();
        assert_eq!(
            c.drain_reply(),
            b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n"
        );
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        architect packet (PORTING.md §2 #5 + §4.5 reply mapping)
//   target_crate:  redis-core
//   confidence:    high
//   todos:         1
//   port_notes:    1
//   unsafe_blocks: 0
//   notes:         Reply writer + arg access. RedisServer reference deferred to Phase 3.
// ──────────────────────────────────────────────────────────────────────────
