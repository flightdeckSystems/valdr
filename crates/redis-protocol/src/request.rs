//! RESP2 incremental request parser.
//!
//! Parses bytes flowing from a client to the server. Supports the two
//! historical request encodings used by Redis clients:
//!
//! * **Inline** — whitespace-separated tokens terminated by `\r\n`. Used by
//!   telnet/`redis-cli` for quick tests.
//! * **Multibulk** — RESP2 array of bulk strings: `*N\r\n$L\r\n<bytes>\r\n…`.
//!   The wire encoding used by every modern client library.
//!
//! C reference: `networking.c::processInlineBuffer` and
//! `networking.c::processMultibulkBuffer`.
//!
//! # Contract
//!
//! [`parse_inline_or_multibulk`] is the entry point. It returns:
//!
//! * `Ok(Some((argv, consumed)))` — a complete command was parsed. `consumed`
//!   is the number of bytes from `buf` that should be drained.
//! * `Ok(None)` — the buffer holds a partial command; the caller should keep
//!   reading and call again.
//! * `Err(RedisError)` — a protocol error. The caller should close the
//!   connection.

use redis_types::{RedisError, RedisString};

/// Maximum inline command length (matches C `PROTO_INLINE_MAX_SIZE`, 64 KiB).
pub const PROTO_INLINE_MAX_SIZE: usize = 64 * 1024;

/// Maximum number of arguments in a multibulk command (matches C
/// `PROTO_REQ_MULTIBULK_MAX_LEN`, 1 million).
pub const PROTO_REQ_MULTIBULK_MAX_LEN: i64 = 1_000_000;

/// Maximum bulk-string payload length, also bounds inline tokens.
/// 512 MiB matches the C `PROTO_MAX_BULK_LEN` default.
pub const PROTO_MAX_BULK_LEN: i64 = 512 * 1024 * 1024;

/// Parse one complete RESP2 request from `buf`.
///
/// Sniffs the first byte: `*` selects the multibulk path, anything else falls
/// through to inline. Returns `Ok(None)` if the buffer does not yet hold a
/// complete frame.
pub fn parse_inline_or_multibulk(
    buf: &[u8],
) -> Result<Option<(Vec<RedisString>, usize)>, RedisError> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] == b'*' {
        parse_multibulk(buf)
    } else {
        parse_inline(buf)
    }
}

/// Parse a multibulk request: `*N\r\n$L\r\n<bytes>\r\n…`.
///
/// C: `networking.c::processMultibulkBuffer`.
fn parse_multibulk(buf: &[u8]) -> Result<Option<(Vec<RedisString>, usize)>, RedisError> {
    let mut pos: usize = 1;

    let (argc, after_argc) = match read_signed_line(buf, pos)? {
        Some(v) => v,
        None => return Ok(None),
    };
    pos = after_argc;

    if argc <= 0 {
        if argc < -1 {
            return Err(RedisError::runtime(b"Protocol error: invalid multibulk length"));
        }
        return Ok(Some((Vec::new(), pos)));
    }
    if argc > PROTO_REQ_MULTIBULK_MAX_LEN {
        return Err(RedisError::runtime(b"Protocol error: invalid multibulk length"));
    }

    let mut argv: Vec<RedisString> = Vec::with_capacity(argc as usize);

    for _ in 0..argc {
        if pos >= buf.len() {
            return Ok(None);
        }
        if buf[pos] != b'$' {
            return Err(RedisError::runtime(
                b"Protocol error: expected '$', got something else",
            ));
        }
        pos += 1;

        let (bulklen, after_len) = match read_signed_line(buf, pos)? {
            Some(v) => v,
            None => return Ok(None),
        };
        pos = after_len;

        if bulklen < 0 || bulklen > PROTO_MAX_BULK_LEN {
            return Err(RedisError::runtime(b"Protocol error: invalid bulk length"));
        }
        let bulklen = bulklen as usize;

        let payload_end = pos
            .checked_add(bulklen)
            .ok_or_else(|| RedisError::runtime(b"Protocol error: bulk length overflow"))?;
        let frame_end = payload_end
            .checked_add(2)
            .ok_or_else(|| RedisError::runtime(b"Protocol error: bulk length overflow"))?;
        if frame_end > buf.len() {
            return Ok(None);
        }
        if &buf[payload_end..frame_end] != b"\r\n" {
            return Err(RedisError::runtime(
                b"Protocol error: bulk payload not terminated by CRLF",
            ));
        }

        argv.push(RedisString::from_bytes(&buf[pos..payload_end]));
        pos = frame_end;
    }

    Ok(Some((argv, pos)))
}

/// Parse an inline command: whitespace-separated tokens ending in `\r\n` or `\n`.
///
/// C: `networking.c::processInlineBuffer`.
fn parse_inline(buf: &[u8]) -> Result<Option<(Vec<RedisString>, usize)>, RedisError> {
    let newline = match buf.iter().position(|&b| b == b'\n') {
        Some(n) => n,
        None => {
            if buf.len() > PROTO_INLINE_MAX_SIZE {
                return Err(RedisError::runtime(
                    b"Protocol error: too big inline request",
                ));
            }
            return Ok(None);
        }
    };

    let line_end = if newline > 0 && buf[newline - 1] == b'\r' {
        newline - 1
    } else {
        newline
    };
    let line = &buf[..line_end];

    let argv = split_inline_tokens(line)?;
    Ok(Some((argv, newline + 1)))
}

/// Read an ASCII signed decimal integer terminated by `\r\n` starting at `pos`.
///
/// Returns `Ok(Some((value, new_pos)))` where `new_pos` is the byte index after
/// the trailing CRLF; returns `Ok(None)` if the CRLF has not yet arrived.
fn read_signed_line(buf: &[u8], pos: usize) -> Result<Option<(i64, usize)>, RedisError> {
    if pos >= buf.len() {
        return Ok(None);
    }
    let cr_offset = match buf[pos..].iter().position(|&b| b == b'\r') {
        Some(o) => o,
        None => return Ok(None),
    };
    let cr_idx = pos + cr_offset;
    if cr_idx + 1 >= buf.len() {
        return Ok(None);
    }
    if buf[cr_idx + 1] != b'\n' {
        return Err(RedisError::runtime(b"Protocol error: expected LF after CR"));
    }
    let value = parse_i64_ascii(&buf[pos..cr_idx])?;
    Ok(Some((value, cr_idx + 2)))
}

/// Parse an ASCII decimal `i64` from `bytes`. Empty input is an error.
fn parse_i64_ascii(bytes: &[u8]) -> Result<i64, RedisError> {
    if bytes.is_empty() {
        return Err(RedisError::runtime(b"Protocol error: empty integer field"));
    }
    let (negative, digits) = if bytes[0] == b'-' {
        (true, &bytes[1..])
    } else if bytes[0] == b'+' {
        (false, &bytes[1..])
    } else {
        (false, bytes)
    };
    if digits.is_empty() {
        return Err(RedisError::runtime(b"Protocol error: empty integer field"));
    }
    let mut acc: i64 = 0;
    for &b in digits {
        let d = match b {
            b'0'..=b'9' => (b - b'0') as i64,
            _ => {
                return Err(RedisError::runtime(
                    b"Protocol error: non-digit in integer field",
                ));
            }
        };
        acc = acc
            .checked_mul(10)
            .and_then(|v| v.checked_add(d))
            .ok_or_else(|| RedisError::runtime(b"Protocol error: integer overflow"))?;
    }
    Ok(if negative { -acc } else { acc })
}

/// Split an inline command line into argv tokens.
///
/// Handles double-quoted strings with `\x`-escapes the same way the C
/// `splitArgs` helper does. For Wave A the simple whitespace-split branch is
/// sufficient (no quoted-string support); proper escape handling lands when
/// `sds::sdssplitargs` is ported.
///
/// TODO(architect): port the full `sdssplitargs` escape handling. Until then
/// quoted-arg inline commands (rare in practice) will be tokenised by
/// whitespace only.
fn split_inline_tokens(line: &[u8]) -> Result<Vec<RedisString>, RedisError> {
    let mut argv: Vec<RedisString> = Vec::new();
    let mut i = 0;
    while i < line.len() {
        while i < line.len() && is_inline_whitespace(line[i]) {
            i += 1;
        }
        if i >= line.len() {
            break;
        }
        let start = i;
        while i < line.len() && !is_inline_whitespace(line[i]) {
            i += 1;
        }
        argv.push(RedisString::from_bytes(&line[start..i]));
    }
    Ok(argv)
}

fn is_inline_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_is_incomplete() {
        assert!(matches!(parse_inline_or_multibulk(b""), Ok(None)));
    }

    #[test]
    fn inline_ping() {
        let (argv, n) = parse_inline_or_multibulk(b"PING\r\n").unwrap().unwrap();
        assert_eq!(n, 6);
        assert_eq!(argv.len(), 1);
        assert_eq!(argv[0].as_bytes(), b"PING");
    }

    #[test]
    fn inline_with_args_lf_only() {
        let (argv, n) = parse_inline_or_multibulk(b"SET foo bar\n").unwrap().unwrap();
        assert_eq!(n, 12);
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[0].as_bytes(), b"SET");
        assert_eq!(argv[1].as_bytes(), b"foo");
        assert_eq!(argv[2].as_bytes(), b"bar");
    }

    #[test]
    fn multibulk_ping() {
        let buf = b"*1\r\n$4\r\nPING\r\n";
        let (argv, n) = parse_inline_or_multibulk(buf).unwrap().unwrap();
        assert_eq!(n, buf.len());
        assert_eq!(argv.len(), 1);
        assert_eq!(argv[0].as_bytes(), b"PING");
    }

    #[test]
    fn multibulk_set_command() {
        let buf = b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
        let (argv, n) = parse_inline_or_multibulk(buf).unwrap().unwrap();
        assert_eq!(n, buf.len());
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[0].as_bytes(), b"SET");
        assert_eq!(argv[1].as_bytes(), b"foo");
        assert_eq!(argv[2].as_bytes(), b"bar");
    }

    #[test]
    fn multibulk_partial_header_returns_none() {
        assert!(matches!(parse_inline_or_multibulk(b"*3\r"), Ok(None)));
    }

    #[test]
    fn multibulk_partial_payload_returns_none() {
        let buf = b"*3\r\n$3\r\nSET\r\n$3\r\nfo";
        assert!(matches!(parse_inline_or_multibulk(buf), Ok(None)));
    }

    #[test]
    fn multibulk_zero_args_consumes_header() {
        let (argv, n) = parse_inline_or_multibulk(b"*0\r\n").unwrap().unwrap();
        assert_eq!(n, 4);
        assert!(argv.is_empty());
    }

    #[test]
    fn multibulk_rejects_bad_bulk_length() {
        let err = parse_inline_or_multibulk(b"*1\r\n$-2\r\n").unwrap_err();
        assert!(matches!(err, RedisError::Runtime(_)));
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        architect packet (Wave A — request-side RESP parser)
//   target_crate:  redis-protocol
//   confidence:    high
//   todos:         1
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         Mirrors processInlineBuffer + processMultibulkBuffer.
//                  No quoted-string escape handling yet; proper sdssplitargs
//                  port flagged as TODO(architect).
// ──────────────────────────────────────────────────────────────────────────
