//! `RespFrame` — Rust enum representing one RESP2/RESP3 wire frame.
//!
//! Per PORTING.md §2 #2. RESP2 variants land now (Simple, Error,
//! Integer, Bulk, Array, Null). RESP3 variants stubbed; encoders /
//! decoders for them are todo!() until Phase 2 protocol translation
//! packets land.
//!
//! `Bulk(None)` represents a RESP2 null bulk string (`$-1\r\n`); the
//! dedicated `Null` variant is the RESP3 null (`_\r\n`).

use redis_types::RedisString;

/// Note: not `Eq` because `Double(f64)` can be NaN. Use `PartialEq` for
/// comparisons; tests should not put RespFrame in a HashSet or use it as
/// a HashMap key unless they exclude RESP3 Double frames.
#[derive(Debug, Clone, PartialEq)]
pub enum RespFrame {
    // ── RESP2 (Phase 2) ───────────────────────────────────────────
    /// `+OK\r\n` — simple string. Bytes excluding the leading `+` and trailing CRLF.
    Simple(RedisString),
    /// `-ERR ...\r\n` — error line. Bytes excluding the leading `-` and trailing CRLF.
    Error(RedisString),
    /// `:<n>\r\n` — integer.
    Integer(i64),
    /// `$<len>\r\n<bytes>\r\n` or `$-1\r\n` (None).
    Bulk(Option<RedisString>),
    /// `*<n>\r\n<frame>...` or `*-1\r\n` (None).
    Array(Option<Vec<RespFrame>>),

    // ── RESP3 (Phase 2 or later) ──────────────────────────────────
    /// `_\r\n` — RESP3 explicit null.
    Null,
    /// `#t\r\n` / `#f\r\n` — RESP3 boolean.
    Boolean(bool),
    /// `,<repr>\r\n` — RESP3 double.
    Double(f64),
    /// `(<digits>\r\n` — RESP3 big number.
    BigNumber(RedisString),
    /// `!<len>\r\n<msg>\r\n` — RESP3 bulk-style error.
    BulkError(RedisString),
    /// `=<len>\r\n<3chars>:<bytes>\r\n` — RESP3 verbatim string with format tag.
    VerbatimString { format: [u8; 3], data: RedisString },
    /// `%<n>\r\n<key>\r\n<value>\r\n...` — RESP3 map.
    Map(Vec<(RespFrame, RespFrame)>),
    /// `~<n>\r\n<frame>...` — RESP3 set.
    Set(Vec<RespFrame>),
    /// `|<n>\r\n<key>\r\n<value>\r\n...` — RESP3 attribute (out-of-band).
    Attribute(Vec<(RespFrame, RespFrame)>),
    /// `><n>\r\n<frame>...` — RESP3 push (server-initiated).
    Push(Vec<RespFrame>),
}

impl RespFrame {
    pub fn simple(s: impl Into<RedisString>) -> Self {
        RespFrame::Simple(s.into())
    }

    pub fn error(s: impl Into<RedisString>) -> Self {
        RespFrame::Error(s.into())
    }

    pub fn integer(n: i64) -> Self {
        RespFrame::Integer(n)
    }

    pub fn bulk(s: impl Into<RedisString>) -> Self {
        RespFrame::Bulk(Some(s.into()))
    }

    pub fn null_bulk() -> Self {
        RespFrame::Bulk(None)
    }

    pub fn array(items: Vec<RespFrame>) -> Self {
        RespFrame::Array(Some(items))
    }

    pub fn null_array() -> Self {
        RespFrame::Array(None)
    }
}

/// Encode a RespFrame onto the wire as RESP2 bytes.
///
/// Phase 2 deliverable. RESP3 variants will panic via todo!() until
/// the translator/architect lands a RESP3 encoder.
pub fn encode_resp2(frame: &RespFrame, buf: &mut Vec<u8>) {
    use std::io::Write;
    match frame {
        RespFrame::Simple(s) => {
            buf.push(b'+');
            buf.extend_from_slice(s.as_bytes());
            buf.extend_from_slice(b"\r\n");
        }
        RespFrame::Error(s) => {
            buf.push(b'-');
            buf.extend_from_slice(s.as_bytes());
            buf.extend_from_slice(b"\r\n");
        }
        RespFrame::Integer(n) => {
            buf.push(b':');
            let _ = write!(buf, "{}", n);
            buf.extend_from_slice(b"\r\n");
        }
        RespFrame::Bulk(None) => {
            buf.extend_from_slice(b"$-1\r\n");
        }
        RespFrame::Bulk(Some(data)) => {
            buf.push(b'$');
            let _ = write!(buf, "{}", data.len());
            buf.extend_from_slice(b"\r\n");
            buf.extend_from_slice(data.as_bytes());
            buf.extend_from_slice(b"\r\n");
        }
        RespFrame::Array(None) => {
            buf.extend_from_slice(b"*-1\r\n");
        }
        RespFrame::Array(Some(items)) => {
            buf.push(b'*');
            let _ = write!(buf, "{}", items.len());
            buf.extend_from_slice(b"\r\n");
            for it in items {
                encode_resp2(it, buf);
            }
        }
        // RESP3 — translator will fill in when Phase 2 lands. For now, allow
        // construction of these variants but fail loudly on encoding so we
        // don't accidentally ship a server that silently drops RESP3 features.
        RespFrame::Null
        | RespFrame::Boolean(_)
        | RespFrame::Double(_)
        | RespFrame::BigNumber(_)
        | RespFrame::BulkError(_)
        | RespFrame::VerbatimString { .. }
        | RespFrame::Map(_)
        | RespFrame::Set(_)
        | RespFrame::Attribute(_)
        | RespFrame::Push(_) => {
            todo!("RESP3 frame encoding not yet implemented; needs translation packet")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(frame: RespFrame) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_resp2(&frame, &mut buf);
        buf
    }

    #[test]
    fn simple_ok() {
        assert_eq!(enc(RespFrame::simple(b"OK".as_slice())), b"+OK\r\n");
    }

    #[test]
    fn error_line() {
        assert_eq!(enc(RespFrame::error(b"ERR foo".as_slice())), b"-ERR foo\r\n");
    }

    #[test]
    fn integer_zero_and_negative() {
        assert_eq!(enc(RespFrame::integer(0)), b":0\r\n");
        assert_eq!(enc(RespFrame::integer(-42)), b":-42\r\n");
    }

    #[test]
    fn bulk_with_bytes() {
        assert_eq!(enc(RespFrame::bulk(b"hi".as_slice())), b"$2\r\nhi\r\n");
    }

    #[test]
    fn null_bulk_resp2() {
        assert_eq!(enc(RespFrame::null_bulk()), b"$-1\r\n");
    }

    #[test]
    fn empty_array() {
        assert_eq!(enc(RespFrame::array(vec![])), b"*0\r\n");
    }

    #[test]
    fn nested_array() {
        let f = RespFrame::array(vec![RespFrame::integer(1), RespFrame::bulk(b"x".as_slice())]);
        assert_eq!(enc(f), b"*2\r\n:1\r\n$1\r\nx\r\n");
    }

    #[test]
    fn bulk_round_trips_non_utf8() {
        let bytes = vec![0xff, 0x00, 0xfe];
        let f = RespFrame::bulk(bytes.clone());
        let out = enc(f);
        assert_eq!(out, [b"$3\r\n".as_slice(), &bytes[..], b"\r\n"].concat());
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        architect packet (PORTING.md §2 #2)
//   target_crate:  redis-protocol
//   confidence:    high
//   todos:         0
//   port_notes:    1
//   unsafe_blocks: 0
//   notes:         RESP2 encoder complete; RESP3 variants present, encoder is todo!() (translator packet).
// ──────────────────────────────────────────────────────────────────────────
