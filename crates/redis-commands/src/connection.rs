//! Connection-management and server commands: PING, ECHO, SELECT, CLIENT,
//! COMMAND, DEBUG, TIME, HELLO, RESET, QUIT.
//!
//! Most handlers operate purely against the client's argv and reply buffer;
//! they never need to touch the keyspace.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use redis_core::CommandContext;
use redis_protocol::frame::RespFrame;
use redis_types::{RedisError, RedisResult, RedisString};

/// `PING [message]`.
///
/// With zero user arguments, replies with the simple string `+PONG\r\n`.
/// With exactly one user argument, replies with that argument as a bulk
/// string (mirroring the real Redis behaviour). Any larger arity is a
/// wrong-arity error.
pub fn ping_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    match ctx.arg_count() {
        1 => ctx.reply_simple_string(b"PONG"),
        2 => {
            let msg = ctx.arg_owned(1usize)?;
            ctx.reply_bulk_string(msg)
        }
        _ => Err(RedisError::wrong_number_of_args(b"ping")),
    }
}

/// `ECHO message`.
///
/// Echoes its single argument back as a bulk string. Any other arity is a
/// wrong-arity error.
pub fn echo_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() != 2 {
        return Err(RedisError::wrong_number_of_args(b"echo"));
    }
    let msg = ctx.arg_owned(1usize)?;
    ctx.reply_bulk_string(msg)
}

/// `SELECT index`.
///
/// The pilot server is single-DB. `SELECT 0` succeeds with `+OK\r\n`; any
/// other index is rejected as out of range. When a real multi-DB server
/// lands the index range will widen to `0..databases`.
pub fn select_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() != 2 {
        return Err(RedisError::wrong_number_of_args(b"select"));
    }
    let raw = ctx.arg_owned(1usize)?;
    let idx = parse_i64_strict(raw.as_bytes())
        .ok_or_else(|| RedisError::runtime(b"ERR value is not an integer or out of range"))?;
    if idx != 0 {
        return Err(RedisError::runtime(b"ERR DB index is out of range"));
    }
    ctx.client_mut().db_index = 0;
    ctx.reply_simple_string(b"OK")
}

/// `TIME`.
///
/// Replies with a two-element array of bulk strings: the current Unix time
/// in seconds and the microseconds component within the current second.
/// Read directly from `SystemTime::now()`.
pub fn time_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() != 1 {
        return Err(RedisError::wrong_number_of_args(b"time"));
    }
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| RedisError::runtime(b"ERR system clock before unix epoch"))?;
    let secs = dur.as_secs();
    let micros = dur.subsec_micros();
    let secs_bytes = format_u64_decimal(secs);
    let micros_bytes = format_u64_decimal(micros as u64);
    let frame = RespFrame::array(vec![
        RespFrame::bulk(RedisString::from_vec(secs_bytes)),
        RespFrame::bulk(RedisString::from_vec(micros_bytes)),
    ]);
    ctx.reply_frame(&frame)
}

/// `QUIT`.
///
/// Replies `+OK\r\n` then asks the accept loop to drop the connection by
/// setting `client.should_close`. The accept loop flushes the reply before
/// closing.
pub fn quit_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() != 1 {
        return Err(RedisError::wrong_number_of_args(b"quit"));
    }
    ctx.client_mut().should_close = true;
    ctx.reply_simple_string(b"OK")
}

/// `RESET`.
///
/// Resets the client's transient state (name, MULTI state, db, flags, queued
/// reply) and replies `+RESET\r\n`.
pub fn reset_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() != 1 {
        return Err(RedisError::wrong_number_of_args(b"reset"));
    }
    ctx.client_mut().reset_state();
    ctx.reply_simple_string(b"RESET")
}

/// `DEBUG <subcommand> [args]`.
///
/// Pilot subset:
///   * `DEBUG SLEEP seconds` — sleep for the given (fractional) seconds,
///     then reply `+OK\r\n`. Used by tests to inject latency.
///
/// Any other subcommand falls through to an `ERR DEBUG ...` error.
pub fn debug_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() < 2 {
        return Err(RedisError::wrong_number_of_args(b"debug"));
    }
    let sub = ctx.arg_owned(1usize)?;
    if ascii_eq_ignore_case(sub.as_bytes(), b"SLEEP") {
        if ctx.arg_count() != 3 {
            return Err(RedisError::wrong_number_of_args(b"debug"));
        }
        let secs_arg = ctx.arg_owned(2usize)?;
        let secs = parse_f64_strict(secs_arg.as_bytes())
            .ok_or_else(|| RedisError::runtime(b"ERR value is not a valid float"))?;
        if secs.is_sign_negative() || secs.is_nan() {
            return Err(RedisError::runtime(b"ERR value is not a valid float"));
        }
        let dur = std::time::Duration::from_secs_f64(secs);
        std::thread::sleep(dur);
        return ctx.reply_simple_string(b"OK");
    }
    let mut msg = Vec::with_capacity(b"ERR Unknown DEBUG subcommand: ".len() + sub.as_bytes().len());
    msg.extend_from_slice(b"ERR Unknown DEBUG subcommand: ");
    msg.extend_from_slice(sub.as_bytes());
    Err(RedisError::runtime(msg))
}

/// `HELLO [protover] [AUTH user pass] [SETNAME name]`.
///
/// Pilot-shape reply: a flat RESP2 multi-bulk of `[key, value]` pairs
/// describing the server. Returns a list (not a RESP3 map) regardless of
/// the requested protocol version; the underlying client representation is
/// still RESP2. AUTH and SETNAME options parse-and-ignore for now — the
/// SETNAME option does set the client name when present.
pub fn hello_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    let argc = ctx.arg_count();
    let mut proto: i32 = 2;
    let mut i = 1usize;
    if argc > 1 {
        let first = ctx.arg_owned(1usize)?;
        if !ascii_eq_ignore_case(first.as_bytes(), b"AUTH")
            && !ascii_eq_ignore_case(first.as_bytes(), b"SETNAME")
        {
            let parsed = parse_i64_strict(first.as_bytes())
                .ok_or_else(|| RedisError::runtime(b"NOPROTO unsupported protocol version"))?;
            if parsed != 2 && parsed != 3 {
                return Err(RedisError::runtime(b"NOPROTO unsupported protocol version"));
            }
            proto = parsed as i32;
            i = 2;
        }
    }
    while i < argc {
        let tok = ctx.arg_owned(i)?;
        if ascii_eq_ignore_case(tok.as_bytes(), b"AUTH") {
            if argc < i + 3 {
                return Err(RedisError::syntax(b"Syntax error in HELLO"));
            }
            i += 3;
        } else if ascii_eq_ignore_case(tok.as_bytes(), b"SETNAME") {
            if argc < i + 2 {
                return Err(RedisError::syntax(b"Syntax error in HELLO"));
            }
            let name = ctx.arg_owned(i + 1)?;
            validate_client_name(name.as_bytes())?;
            ctx.client_mut().name = Some(name);
            i += 2;
        } else {
            return Err(RedisError::syntax(b"Syntax error in HELLO"));
        }
    }
    if proto == 3 {
        return Err(RedisError::runtime(b"NOPROTO RESP3 not yet supported"));
    }
    ctx.client_mut().resp_proto = proto;
    let id = ctx.client_ref().id();
    let id_bytes = format_u64_decimal(id);
    let mut items: Vec<RespFrame> = Vec::with_capacity(14);
    items.push(RespFrame::bulk(RedisString::from_bytes(b"server")));
    items.push(RespFrame::bulk(RedisString::from_bytes(b"redis")));
    items.push(RespFrame::bulk(RedisString::from_bytes(b"version")));
    items.push(RespFrame::bulk(RedisString::from_bytes(b"7.0.0")));
    items.push(RespFrame::bulk(RedisString::from_bytes(b"proto")));
    items.push(RespFrame::Integer(proto as i64));
    items.push(RespFrame::bulk(RedisString::from_bytes(b"id")));
    items.push(RespFrame::bulk(RedisString::from_vec(id_bytes)));
    items.push(RespFrame::bulk(RedisString::from_bytes(b"mode")));
    items.push(RespFrame::bulk(RedisString::from_bytes(b"standalone")));
    items.push(RespFrame::bulk(RedisString::from_bytes(b"role")));
    items.push(RespFrame::bulk(RedisString::from_bytes(b"master")));
    items.push(RespFrame::bulk(RedisString::from_bytes(b"modules")));
    items.push(RespFrame::array(Vec::new()));
    ctx.reply_frame(&RespFrame::array(items))
}

/// `CLIENT <subcommand> [args]`.
///
/// Pilot subset:
///   * `CLIENT ID` — integer reply of the client's connection id.
///   * `CLIENT GETNAME` — bulk reply of the stored name (empty bulk when unset).
///   * `CLIENT SETNAME name` — store the name; replies `+OK\r\n`.
///   * `CLIENT NO-EVICT ON|OFF` — no-op, replies `+OK\r\n`.
///   * `CLIENT NO-TOUCH ON|OFF` — no-op, replies `+OK\r\n`.
///   * `CLIENT LIST` — single-line description of the current client.
pub fn client_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() < 2 {
        return Err(RedisError::wrong_number_of_args(b"client"));
    }
    let sub = ctx.arg_owned(1usize)?;
    let sub_bytes = sub.as_bytes();
    if ascii_eq_ignore_case(sub_bytes, b"ID") {
        if ctx.arg_count() != 2 {
            return Err(RedisError::wrong_number_of_args(b"client|id"));
        }
        let id = ctx.client_ref().id() as i64;
        return ctx.reply_integer(id);
    }
    if ascii_eq_ignore_case(sub_bytes, b"GETNAME") {
        if ctx.arg_count() != 2 {
            return Err(RedisError::wrong_number_of_args(b"client|getname"));
        }
        let payload = match &ctx.client_ref().name {
            Some(n) => n.clone(),
            None => RedisString::new(),
        };
        return ctx.reply_bulk_string(payload);
    }
    if ascii_eq_ignore_case(sub_bytes, b"SETNAME") {
        if ctx.arg_count() != 3 {
            return Err(RedisError::wrong_number_of_args(b"client|setname"));
        }
        let name = ctx.arg_owned(2usize)?;
        validate_client_name(name.as_bytes())?;
        ctx.client_mut().name = Some(name);
        return ctx.reply_simple_string(b"OK");
    }
    if ascii_eq_ignore_case(sub_bytes, b"NO-EVICT") {
        if ctx.arg_count() != 3 {
            return Err(RedisError::wrong_number_of_args(b"client|no-evict"));
        }
        let flag = ctx.arg_owned(2usize)?;
        if !ascii_eq_ignore_case(flag.as_bytes(), b"ON")
            && !ascii_eq_ignore_case(flag.as_bytes(), b"OFF")
        {
            return Err(RedisError::syntax(b""));
        }
        return ctx.reply_simple_string(b"OK");
    }
    if ascii_eq_ignore_case(sub_bytes, b"NO-TOUCH") {
        if ctx.arg_count() != 3 {
            return Err(RedisError::wrong_number_of_args(b"client|no-touch"));
        }
        let flag = ctx.arg_owned(2usize)?;
        if !ascii_eq_ignore_case(flag.as_bytes(), b"ON")
            && !ascii_eq_ignore_case(flag.as_bytes(), b"OFF")
        {
            return Err(RedisError::syntax(b""));
        }
        return ctx.reply_simple_string(b"OK");
    }
    if ascii_eq_ignore_case(sub_bytes, b"LIST") {
        let line = build_client_list_line(ctx);
        return ctx.reply_bulk_string(RedisString::from_vec(line));
    }
    let mut msg = Vec::with_capacity(b"ERR Unknown CLIENT subcommand: ".len() + sub_bytes.len());
    msg.extend_from_slice(b"ERR Unknown CLIENT subcommand: ");
    msg.extend_from_slice(sub_bytes);
    Err(RedisError::runtime(msg))
}

/// `COMMAND` / `COMMAND COUNT`.
///
/// `COMMAND` (no args) replies with an array of bulk-string command names
/// drawn from the dispatch table. This stub omits the per-command metadata
/// (arity/flags/key-positions/etc.); `redis-cli` accepts a names-only reply.
///
/// `COMMAND COUNT` replies with the integer length of the dispatch table.
pub fn command_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() == 1 {
        let handlers = crate::dispatch::HANDLERS;
        let mut items: Vec<RespFrame> = Vec::with_capacity(handlers.len());
        for entry in handlers.iter() {
            items.push(RespFrame::bulk(RedisString::from_bytes(entry.name)));
        }
        return ctx.reply_frame(&RespFrame::array(items));
    }
    let sub = ctx.arg_owned(1usize)?;
    if ascii_eq_ignore_case(sub.as_bytes(), b"COUNT") {
        if ctx.arg_count() != 2 {
            return Err(RedisError::wrong_number_of_args(b"command|count"));
        }
        let n = crate::dispatch::HANDLERS.len() as i64;
        return ctx.reply_integer(n);
    }
    let mut msg = Vec::with_capacity(b"ERR Unknown COMMAND subcommand: ".len() + sub.as_bytes().len());
    msg.extend_from_slice(b"ERR Unknown COMMAND subcommand: ");
    msg.extend_from_slice(sub.as_bytes());
    Err(RedisError::runtime(msg))
}

/// Validate a client name per Redis rules: no spaces, newlines, or other
/// whitespace/control characters.
fn validate_client_name(name: &[u8]) -> RedisResult<()> {
    for &b in name {
        if b <= 0x20 || b == 0x7f {
            return Err(RedisError::runtime(
                b"ERR Client names cannot contain spaces, newlines or special characters.",
            ));
        }
    }
    Ok(())
}

/// Build the single-line description used by `CLIENT LIST`.
fn build_client_list_line(ctx: &CommandContext<'_>) -> Vec<u8> {
    let mut line: Vec<u8> = Vec::with_capacity(128);
    let client = ctx.client_ref();
    let _ = write!(line, "id={} addr=", client.id());
    match &client.addr {
        Some(s) => line.extend_from_slice(s.as_bytes()),
        None => line.extend_from_slice(b""),
    }
    line.extend_from_slice(b" name=");
    if let Some(n) = &client.name {
        line.extend_from_slice(n.as_bytes());
    }
    let _ = write!(line, " db={}", client.db_index);
    line
}

/// Case-insensitive ASCII equality.
fn ascii_eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|(x, y)| ascii_lower(*x) == ascii_lower(*y))
}

fn ascii_lower(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 32
    } else {
        b
    }
}

/// Parse an ASCII decimal integer with optional leading `-`. Rejects empty
/// input, leading/trailing whitespace, plus signs, and non-digit bytes.
fn parse_i64_strict(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    let s = std::str::from_utf8(bytes).ok()?;
    s.parse::<i64>().ok()
}

/// Parse a floating-point number. Rejects empty input, whitespace, and
/// non-numeric bytes.
fn parse_f64_strict(bytes: &[u8]) -> Option<f64> {
    if bytes.is_empty() {
        return None;
    }
    let s = std::str::from_utf8(bytes).ok()?;
    s.parse::<f64>().ok()
}

/// Decimal-encode `n` as ASCII bytes.
fn format_u64_decimal(n: u64) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::with_capacity(20);
    let _ = write!(buf, "{}", n);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis_core::Client;
    use redis_types::RedisString;

    #[test]
    fn ping_no_args_replies_pong() {
        let mut c = Client::new(1);
        c.set_args(vec![RedisString::from_bytes(b"PING")]);
        let mut ctx = CommandContext::new(&mut c);
        ping_command(&mut ctx).unwrap();
        assert_eq!(c.drain_reply(), b"+PONG\r\n");
    }

    #[test]
    fn ping_with_message_replies_bulk() {
        let mut c = Client::new(1);
        c.set_args(vec![
            RedisString::from_bytes(b"PING"),
            RedisString::from_bytes(b"world"),
        ]);
        let mut ctx = CommandContext::new(&mut c);
        ping_command(&mut ctx).unwrap();
        assert_eq!(c.drain_reply(), b"$5\r\nworld\r\n");
    }

    #[test]
    fn ping_too_many_args_is_arity_error() {
        let mut c = Client::new(1);
        c.set_args(vec![
            RedisString::from_bytes(b"PING"),
            RedisString::from_bytes(b"a"),
            RedisString::from_bytes(b"b"),
        ]);
        let mut ctx = CommandContext::new(&mut c);
        let err = ping_command(&mut ctx).unwrap_err();
        match err {
            RedisError::WrongNumberOfArgs(name) => {
                assert_eq!(name.as_bytes(), b"ping");
            }
            _ => panic!("expected WrongNumberOfArgs"),
        }
    }

    #[test]
    fn echo_replies_bulk() {
        let mut c = Client::new(1);
        c.set_args(vec![
            RedisString::from_bytes(b"ECHO"),
            RedisString::from_bytes(b"hello"),
        ]);
        let mut ctx = CommandContext::new(&mut c);
        echo_command(&mut ctx).unwrap();
        assert_eq!(c.drain_reply(), b"$5\r\nhello\r\n");
    }

    #[test]
    fn echo_wrong_arity_errors() {
        let mut c = Client::new(1);
        c.set_args(vec![RedisString::from_bytes(b"ECHO")]);
        let mut ctx = CommandContext::new(&mut c);
        let err = echo_command(&mut ctx).unwrap_err();
        match err {
            RedisError::WrongNumberOfArgs(name) => {
                assert_eq!(name.as_bytes(), b"echo");
            }
            _ => panic!("expected WrongNumberOfArgs"),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        translated by hand (Wave B — connection commands)
//   target_crate:  redis-commands
//   confidence:    high
//   todos:         0
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         PING + ECHO. HELLO/AUTH/QUIT remain stubbed in dispatch.
// ──────────────────────────────────────────────────────────────────────────
