//! `EVAL` / `EVALSHA` / `SCRIPT` — server-side Lua scripting.
//!
//! Backed by `mlua` (bundled C Lua 5.1, matching real Redis). The runtime is
//! constructed once per call so global state never leaks across scripts and
//! the dangerous portions of the stdlib (`os`, `io`, `debug`, `require`,
//! `loadfile`, `dofile`, `package`, `print`) are removed before user code
//! runs.
//!
//! `redis.call` / `redis.pcall` re-enter the command dispatch table by
//! saving the client's argv and reply buffer, installing the synthetic
//! argv, calling [`crate::dispatch::dispatch_command_name`], parsing the
//! newly-written reply bytes back into a Lua value, then restoring the
//! caller's argv and the original reply buffer prefix.
//!
//! Script cache is a process-wide `Mutex<HashMap<sha1_hex, bytes>>` keyed
//! by the lower-case 40-byte SHA-1 hex of the source bytes. `SCRIPT LOAD`
//! inserts into the cache; `EVALSHA` looks up; `SCRIPT FLUSH` clears.
//!
//! See `docs/ADR_001_LUA_RUNTIME.md` for the runtime-choice rationale and
//! the full sandbox patch list.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use mlua::{Error as LuaError, Lua, MultiValue, Table as LuaTable, Value as LuaValue};

use redis_core::CommandContext;
use redis_protocol::parser::{ParserCallbacks, ParserCursor};
use redis_types::{RedisError, RedisResult, RedisString};

use crate::dispatch::dispatch_command_name;

/// One captured reply from a `redis.call` re-entry.
///
/// Parsed from the RESP bytes the inner dispatch wrote into `reply_buf`.
/// Used as an intermediate before the value is converted to a Lua value.
#[derive(Debug, Clone)]
enum ReplyValue {
    Nil,
    SimpleString(Vec<u8>),
    Error(Vec<u8>),
    Integer(i64),
    Bulk(Vec<u8>),
    Array(Vec<ReplyValue>),
}

/// Parser-callback adapter that accumulates one RESP frame into a
/// [`ReplyValue`] tree. Built once per inner dispatch and consumed with a
/// single `parse_next` call.
struct ReplyBuilder {
    stack: Vec<Vec<ReplyValue>>,
    pending_lens: Vec<i64>,
    out: Option<ReplyValue>,
    errored: bool,
}

impl ReplyBuilder {
    fn new() -> Self {
        Self { stack: Vec::new(), pending_lens: Vec::new(), out: None, errored: false }
    }

    fn deliver(&mut self, v: ReplyValue) {
        if let Some(top) = self.stack.last_mut() {
            top.push(v);
            let popped = self
                .pending_lens
                .last_mut()
                .map(|n| {
                    *n -= 1;
                    *n
                })
                .unwrap_or(0);
            if popped <= 0 {
                let frame = self.stack.pop().unwrap_or_default();
                self.pending_lens.pop();
                self.deliver(ReplyValue::Array(frame));
            }
        } else {
            self.out = Some(v);
        }
    }
}

impl ParserCallbacks for ReplyBuilder {
    fn on_null_bulk_string(&mut self, _proto: &[u8]) {
        self.deliver(ReplyValue::Nil);
    }
    fn on_null_array(&mut self, _proto: &[u8]) {
        self.deliver(ReplyValue::Nil);
    }
    fn on_bulk_string(&mut self, data: &[u8], _proto: &[u8]) {
        self.deliver(ReplyValue::Bulk(data.to_vec()));
    }
    fn on_error(&mut self, data: &[u8], _proto: &[u8]) {
        self.deliver(ReplyValue::Error(data.to_vec()));
    }
    fn on_simple_str(&mut self, data: &[u8], _proto: &[u8]) {
        self.deliver(ReplyValue::SimpleString(data.to_vec()));
    }
    fn on_long(&mut self, val: i64, _proto: &[u8]) {
        self.deliver(ReplyValue::Integer(val));
    }
    fn on_array(&mut self, cursor: &mut ParserCursor<'_>, len: i64, _proto: &[u8]) {
        if len <= 0 {
            self.deliver(ReplyValue::Array(Vec::new()));
            return;
        }
        self.stack.push(Vec::with_capacity(len as usize));
        self.pending_lens.push(len);
        for _ in 0..len {
            if cursor.parse_next(self).is_err() {
                self.errored = true;
                break;
            }
        }
        if !self.errored {
            let frame = self.stack.pop().unwrap_or_default();
            self.pending_lens.pop();
            self.deliver(ReplyValue::Array(frame));
        }
    }
    fn on_set(&mut self, cursor: &mut ParserCursor<'_>, len: i64, proto: &[u8]) {
        self.on_array(cursor, len, proto);
    }
    fn on_map(&mut self, cursor: &mut ParserCursor<'_>, len: i64, _proto: &[u8]) {
        let pair_count = len.max(0) * 2;
        let mut items: Vec<ReplyValue> = Vec::with_capacity(pair_count as usize);
        for _ in 0..pair_count {
            let mut tmp = ReplyBuilder::new();
            if cursor.parse_next(&mut tmp).is_err() {
                self.errored = true;
                return;
            }
            if let Some(v) = tmp.out {
                items.push(v);
            }
        }
        self.deliver(ReplyValue::Array(items));
    }
    fn on_bool(&mut self, val: bool, _proto: &[u8]) {
        self.deliver(ReplyValue::Integer(if val { 1 } else { 0 }));
    }
    fn on_double(&mut self, val: f64, _proto: &[u8]) {
        self.deliver(ReplyValue::Bulk(format!("{}", val).into_bytes()));
    }
    fn on_big_number(&mut self, data: &[u8], _proto: &[u8]) {
        self.deliver(ReplyValue::Bulk(data.to_vec()));
    }
    fn on_verbatim_string(&mut self, _format: &[u8], data: &[u8], _proto: &[u8]) {
        self.deliver(ReplyValue::Bulk(data.to_vec()));
    }
    fn on_attribute(&mut self, cursor: &mut ParserCursor<'_>, len: i64, _proto: &[u8]) {
        let pair_count = len.max(0) * 2;
        for _ in 0..pair_count {
            let mut tmp = ReplyBuilder::new();
            if cursor.parse_next(&mut tmp).is_err() {
                self.errored = true;
                return;
            }
        }
    }
    fn on_null(&mut self, _proto: &[u8]) {
        self.deliver(ReplyValue::Nil);
    }
    fn on_parse_error(&mut self) {
        self.errored = true;
    }
}

/// Convert a [`ReplyValue`] tree to a Lua value following the Redis Lua
/// semantics: bulk and simple strings become Lua strings, integers become
/// Lua integers, nil becomes Lua nil, errors become `{err = msg}`, arrays
/// become 1-indexed Lua tables.
fn reply_to_lua(lua: &Lua, value: &ReplyValue) -> mlua::Result<LuaValue> {
    match value {
        ReplyValue::Nil => Ok(LuaValue::Boolean(false)),
        ReplyValue::SimpleString(s) => {
            let t = lua.create_table()?;
            t.raw_set("ok", lua.create_string(s)?)?;
            Ok(LuaValue::Table(t))
        }
        ReplyValue::Error(s) => {
            let t = lua.create_table()?;
            t.raw_set("err", lua.create_string(s)?)?;
            Ok(LuaValue::Table(t))
        }
        ReplyValue::Integer(n) => Ok(LuaValue::Integer(*n)),
        ReplyValue::Bulk(b) => Ok(LuaValue::String(lua.create_string(b)?)),
        ReplyValue::Array(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                let v = reply_to_lua(lua, item)?;
                t.raw_set(i as i64 + 1, v)?;
            }
            Ok(LuaValue::Table(t))
        }
    }
}

/// Encode a Lua value as a RESP frame on the wire.
///
/// Mirrors real Redis script-to-protocol conversion: nil → null bulk,
/// integers / numbers → integer (numbers truncated), strings → bulk,
/// booleans → `:1` / null, tables → status if `.ok`, error if `.err`,
/// otherwise a 1-indexed array (terminated at the first nil per Lua-array
/// convention).
fn lua_to_resp(value: &LuaValue, out: &mut Vec<u8>) {
    match value {
        LuaValue::Nil => out.extend_from_slice(b"$-1\r\n"),
        LuaValue::Boolean(true) => out.extend_from_slice(b":1\r\n"),
        LuaValue::Boolean(false) => out.extend_from_slice(b"$-1\r\n"),
        LuaValue::Integer(n) => {
            out.push(b':');
            out.extend_from_slice(n.to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        LuaValue::Number(f) => {
            let n = *f as i64;
            out.push(b':');
            out.extend_from_slice(n.to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        LuaValue::String(s) => {
            let bytes = s.as_bytes();
            out.push(b'$');
            out.extend_from_slice(bytes.len().to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(&bytes);
            out.extend_from_slice(b"\r\n");
        }
        LuaValue::Table(t) => {
            if let Ok(Some(err)) = t.get::<Option<mlua::String>>("err") {
                let bytes = err.as_bytes();
                out.push(b'-');
                if !bytes.starts_with(b"ERR ") && !bytes.iter().take_while(|b| **b != b' ').all(u8::is_ascii_uppercase) {
                    out.extend_from_slice(b"ERR ");
                }
                out.extend_from_slice(&bytes);
                out.extend_from_slice(b"\r\n");
                return;
            }
            if let Ok(Some(ok)) = t.get::<Option<mlua::String>>("ok") {
                let bytes = ok.as_bytes();
                out.push(b'+');
                out.extend_from_slice(&bytes);
                out.extend_from_slice(b"\r\n");
                return;
            }
            let mut items: Vec<LuaValue> = Vec::new();
            let mut i: i64 = 1;
            loop {
                let v: LuaValue = match t.raw_get(i) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                if matches!(v, LuaValue::Nil) {
                    break;
                }
                items.push(v);
                i += 1;
            }
            out.push(b'*');
            out.extend_from_slice(items.len().to_string().as_bytes());
            out.extend_from_slice(b"\r\n");
            for it in &items {
                lua_to_resp(it, out);
            }
        }
        _ => out.extend_from_slice(b"$-1\r\n"),
    }
}

/// Coerce one Lua argument passed to `redis.call(...)` into the byte
/// string the dispatch table expects. Integers/numbers are stringified
/// using Lua's `tostring`-compatible rule (integers stay integral).
fn lua_arg_to_bytes(v: &LuaValue) -> Result<Vec<u8>, LuaError> {
    match v {
        LuaValue::String(s) => Ok(s.as_bytes().to_vec()),
        LuaValue::Integer(n) => Ok(n.to_string().into_bytes()),
        LuaValue::Number(f) => {
            if f.fract() == 0.0 && f.is_finite() {
                Ok(((*f) as i64).to_string().into_bytes())
            } else {
                Ok(format!("{}", f).into_bytes())
            }
        }
        LuaValue::Boolean(true) => Ok(b"1".to_vec()),
        LuaValue::Boolean(false) => Ok(b"0".to_vec()),
        _ => Err(LuaError::RuntimeError(
            "Lua redis() command arguments must be strings or integers".to_string(),
        )),
    }
}

/// Sandbox an `mlua::Lua` instance by removing globals that would let a
/// user script reach the filesystem or the host process. Mirrors the
/// real-Redis sandbox.
fn install_sandbox(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in [
        "os", "io", "debug", "package", "require", "loadfile", "dofile", "print",
    ] {
        globals.set(name, LuaValue::Nil)?;
    }
    Ok(())
}

/// Install `KEYS` and `ARGV` into the per-call Lua globals.
fn install_keys_argv(lua: &Lua, keys: &[RedisString], argv: &[RedisString]) -> mlua::Result<()> {
    let keys_t = lua.create_table()?;
    for (i, k) in keys.iter().enumerate() {
        keys_t.raw_set(i as i64 + 1, lua.create_string(k.as_bytes())?)?;
    }
    lua.globals().set("KEYS", keys_t)?;

    let argv_t = lua.create_table()?;
    for (i, a) in argv.iter().enumerate() {
        argv_t.raw_set(i as i64 + 1, lua.create_string(a.as_bytes())?)?;
    }
    lua.globals().set("ARGV", argv_t)?;
    Ok(())
}

/// Execute one inner command for `redis.call` / `redis.pcall`, capturing
/// the reply bytes the handler appended to `reply_buf` and parsing them
/// back into a [`ReplyValue`].
///
/// Restores the caller's argv and reply prefix unconditionally so the
/// outer EVAL reply is unaffected by inner dispatch side-effects.
fn run_inner_command(
    ctx: &mut CommandContext<'_>,
    args: &[Vec<u8>],
) -> Result<ReplyValue, RedisError> {
    if args.is_empty() {
        return Err(RedisError::runtime(b"ERR wrong number of args calling Redis command"));
    }

    let saved_argv = ctx.client_ref().argv.clone();
    let saved_reply_len = ctx.client_ref().reply_buf.len();

    let new_argv: Vec<RedisString> =
        args.iter().map(|b| RedisString::from_bytes(b.as_slice())).collect();
    ctx.client_mut().set_args(new_argv);

    let name_bytes = args[0].clone();
    let dispatch_result = dispatch_command_name(ctx, &name_bytes);

    let raw_reply: Vec<u8> = {
        let buf = &mut ctx.client_mut().reply_buf;
        let tail = buf.split_off(saved_reply_len);
        tail
    };

    ctx.client_mut().set_args(saved_argv);

    if let Err(err) = dispatch_result {
        if raw_reply.is_empty() {
            return Err(err);
        }
    }

    if raw_reply.is_empty() {
        return Ok(ReplyValue::Nil);
    }

    let mut cursor = ParserCursor::new(&raw_reply);
    let mut builder = ReplyBuilder::new();
    if cursor.parse_next(&mut builder).is_err() || builder.errored {
        return Err(RedisError::runtime(b"ERR could not parse inner reply"));
    }
    builder
        .out
        .ok_or_else(|| RedisError::runtime(b"ERR empty inner reply"))
}

/// Process-wide script cache. Keys are the 40-byte lowercase SHA-1 hex of
/// the source bytes. Values are the source bytes themselves.
fn script_cache() -> &'static Mutex<HashMap<[u8; 40], Vec<u8>>> {
    static CACHE: OnceLock<Mutex<HashMap<[u8; 40], Vec<u8>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `EVAL script numkeys key [key ...] arg [arg ...]`.
///
/// Parses the argv, constructs a fresh sandboxed Lua instance, injects
/// the `redis` table plus `KEYS` / `ARGV`, runs the script, and writes
/// the result back as the outer RESP reply.
pub fn eval_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() < 3 {
        return Err(RedisError::wrong_number_of_args(b"eval"));
    }
    let script = ctx.arg_owned(1usize)?;
    let numkeys = parse_i64(ctx.arg(2usize)?.as_bytes())?;
    if numkeys < 0 {
        return Err(RedisError::runtime(b"ERR Number of keys can't be negative"));
    }
    let numkeys = numkeys as usize;
    let total_extra = ctx.arg_count().saturating_sub(3);
    if numkeys > total_extra {
        return Err(RedisError::runtime(
            b"ERR Number of keys can't be greater than number of args",
        ));
    }
    let mut keys: Vec<RedisString> = Vec::with_capacity(numkeys);
    for i in 0..numkeys {
        keys.push(ctx.arg_owned(3 + i)?);
    }
    let mut argv: Vec<RedisString> = Vec::with_capacity(total_extra - numkeys);
    for i in (3 + numkeys)..ctx.arg_count() {
        argv.push(ctx.arg_owned(i)?);
    }

    run_script(ctx, script.as_bytes(), &keys, &argv)
}

/// `EVALSHA sha1 numkeys key [key ...] arg [arg ...]`.
///
/// Looks up the cached script bytes; falls through to `EVAL` on a hit, or
/// returns the canonical `-NOSCRIPT` reply on a miss.
pub fn evalsha_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() < 3 {
        return Err(RedisError::wrong_number_of_args(b"evalsha"));
    }
    let sha_in = ctx.arg_owned(1usize)?;
    let sha_norm = match normalise_sha(sha_in.as_bytes()) {
        Some(s) => s,
        None => {
            return Err(RedisError::runtime(
                b"NOSCRIPT No matching script. Please use EVAL.",
            ));
        }
    };
    let script_bytes: Option<Vec<u8>> = {
        let guard = match script_cache().lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.get(&sha_norm).cloned()
    };
    let script = match script_bytes {
        Some(b) => b,
        None => {
            return Err(RedisError::runtime(
                b"NOSCRIPT No matching script. Please use EVAL.",
            ));
        }
    };

    let numkeys = parse_i64(ctx.arg(2usize)?.as_bytes())?;
    if numkeys < 0 {
        return Err(RedisError::runtime(b"ERR Number of keys can't be negative"));
    }
    let numkeys = numkeys as usize;
    let total_extra = ctx.arg_count().saturating_sub(3);
    if numkeys > total_extra {
        return Err(RedisError::runtime(
            b"ERR Number of keys can't be greater than number of args",
        ));
    }
    let mut keys: Vec<RedisString> = Vec::with_capacity(numkeys);
    for i in 0..numkeys {
        keys.push(ctx.arg_owned(3 + i)?);
    }
    let mut argv: Vec<RedisString> = Vec::with_capacity(total_extra - numkeys);
    for i in (3 + numkeys)..ctx.arg_count() {
        argv.push(ctx.arg_owned(i)?);
    }

    run_script(ctx, &script, &keys, &argv)
}

/// Shared body of `EVAL` and `EVALSHA`. Creates a fresh Lua state, applies
/// the sandbox, installs `redis`, `KEYS`, `ARGV`, runs the script, and
/// converts the return value to a RESP frame written onto `reply_buf`.
fn run_script(
    ctx: &mut CommandContext<'_>,
    script_bytes: &[u8],
    keys: &[RedisString],
    argv: &[RedisString],
) -> RedisResult<()> {
    let lua = Lua::new();
    install_sandbox(&lua)
        .map_err(|e| RedisError::runtime(format!("ERR Lua sandbox: {}", e).into_bytes()))?;
    install_keys_argv(&lua, keys, argv)
        .map_err(|e| RedisError::runtime(format!("ERR Lua install: {}", e).into_bytes()))?;

    let ctx_cell: RefCell<&mut CommandContext<'_>> = RefCell::new(ctx);

    let script_result: Result<LuaValue, LuaError> = lua.scope(|scope| {
        let redis_tbl = lua.create_table()?;

        let call_fn = {
            let cell = &ctx_cell;
            scope.create_function_mut(move |_lua, args: MultiValue| -> mlua::Result<LuaValue> {
                let arg_bytes = collect_call_args(args)?;
                let mut borrow = cell.borrow_mut();
                match run_inner_command(&mut **borrow, &arg_bytes) {
                    Ok(reply) => {
                        if let ReplyValue::Error(msg) = &reply {
                            return Err(LuaError::RuntimeError(
                                String::from_utf8_lossy(msg).into_owned(),
                            ));
                        }
                        reply_to_lua(_lua, &reply)
                    }
                    Err(e) => Err(LuaError::RuntimeError(
                        String::from_utf8_lossy(e.to_resp_payload().as_bytes()).into_owned(),
                    )),
                }
            })?
        };

        let pcall_fn = {
            let cell = &ctx_cell;
            scope.create_function_mut(move |lua_inner, args: MultiValue| -> mlua::Result<LuaValue> {
                let arg_bytes = collect_call_args(args)?;
                let mut borrow = cell.borrow_mut();
                match run_inner_command(&mut **borrow, &arg_bytes) {
                    Ok(reply) => reply_to_lua(lua_inner, &reply),
                    Err(e) => {
                        let msg = String::from_utf8_lossy(e.to_resp_payload().as_bytes()).into_owned();
                        let t = lua_inner.create_table()?;
                        t.raw_set("err", lua_inner.create_string(&msg)?)?;
                        Ok(LuaValue::Table(t))
                    }
                }
            })?
        };

        let error_reply_fn =
            lua.create_function(|lua_inner, msg: mlua::String| -> mlua::Result<LuaTable> {
                let t = lua_inner.create_table()?;
                t.raw_set("err", msg)?;
                Ok(t)
            })?;

        let status_reply_fn =
            lua.create_function(|lua_inner, msg: mlua::String| -> mlua::Result<LuaTable> {
                let t = lua_inner.create_table()?;
                t.raw_set("ok", msg)?;
                Ok(t)
            })?;

        let sha1hex_fn = lua.create_function(|_lua, s: mlua::String| -> mlua::Result<String> {
            let hex = sha1_hex(&s.as_bytes());
            Ok(String::from_utf8(hex.to_vec()).unwrap_or_default())
        })?;

        let replicate_fn = lua.create_function(|_lua, _: MultiValue| -> mlua::Result<bool> {
            Ok(true)
        })?;

        redis_tbl.raw_set("__raw_call", call_fn)?;
        redis_tbl.raw_set("pcall", pcall_fn)?;
        redis_tbl.raw_set("error_reply", error_reply_fn)?;
        redis_tbl.raw_set("status_reply", status_reply_fn)?;
        redis_tbl.raw_set("sha1hex", sha1hex_fn)?;
        redis_tbl.raw_set("replicate_commands", replicate_fn)?;
        lua.globals().set("redis", redis_tbl)?;

        lua.load(
            "local raw = redis.__raw_call\n\
             redis.call = function(...)\n\
                 local ok, res = pcall(raw, ...)\n\
                 if ok then return res end\n\
                 local msg = tostring(res)\n\
                 msg = msg:gsub(\"^.-: \", \"\", 1)\n\
                 msg = msg:gsub(\"\\nstack traceback.*$\", \"\")\n\
                 error(msg, 0)\n\
             end\n",
        )
        .set_name("redis_call_shim")
        .exec()?;

        lua.load(script_bytes).set_name("user_script").eval::<LuaValue>()
    });

    match script_result {
        Ok(value) => {
            let mut out: Vec<u8> = Vec::new();
            lua_to_resp(&value, &mut out);
            ctx.client_mut().reply_buf.extend_from_slice(&out);
            Ok(())
        }
        Err(LuaError::RuntimeError(msg)) => {
            Err(RedisError::runtime(format!("ERR {}", msg).into_bytes()))
        }
        Err(LuaError::SyntaxError { message, .. }) => {
            Err(RedisError::runtime(
                format!("ERR Error compiling script: {}", message).into_bytes(),
            ))
        }
        Err(e) => Err(RedisError::runtime(
            format!("ERR Error running script: {}", e).into_bytes(),
        )),
    }
}

/// Collect the variadic Lua arguments passed to `redis.call(cmd, ...)`
/// into a byte-string argv suitable for [`run_inner_command`].
fn collect_call_args(args: MultiValue) -> Result<Vec<Vec<u8>>, LuaError> {
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(args.len());
    for v in args {
        out.push(lua_arg_to_bytes(&v)?);
    }
    Ok(out)
}

/// `SCRIPT` subcommand router: LOAD / EXISTS / FLUSH / HELP.
pub fn script_command(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() < 2 {
        return Err(RedisError::wrong_number_of_args(b"script"));
    }
    let sub = ctx.arg_owned(1usize)?;
    let sub_bytes = sub.as_bytes();
    if ascii_eq_ci(sub_bytes, b"LOAD") {
        return script_load(ctx);
    }
    if ascii_eq_ci(sub_bytes, b"EXISTS") {
        return script_exists(ctx);
    }
    if ascii_eq_ci(sub_bytes, b"FLUSH") {
        return script_flush(ctx);
    }
    if ascii_eq_ci(sub_bytes, b"HELP") {
        return script_help(ctx);
    }
    let mut msg = Vec::with_capacity(64 + sub_bytes.len());
    msg.extend_from_slice(b"ERR Unknown SCRIPT subcommand or wrong number of arguments for '");
    msg.extend_from_slice(sub_bytes);
    msg.push(b'\'');
    Err(RedisError::runtime(msg))
}

fn script_load(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() != 3 {
        return Err(RedisError::wrong_number_of_args(b"script|load"));
    }
    let body = ctx.arg_owned(2usize)?;
    let hex = sha1_hex(body.as_bytes());
    {
        let mut guard = match script_cache().lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.insert(hex, body.as_bytes().to_vec());
    }
    ctx.reply_bulk(&hex)
}

fn script_exists(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() < 3 {
        return Err(RedisError::wrong_number_of_args(b"script|exists"));
    }
    let guard = match script_cache().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let n = ctx.arg_count() - 2;
    ctx.reply_array_header(n as i64)?;
    for i in 0..n {
        let raw = ctx.arg_owned(2 + i)?;
        let exists = normalise_sha(raw.as_bytes())
            .map(|h| guard.contains_key(&h))
            .unwrap_or(false);
        ctx.reply_integer(if exists { 1 } else { 0 })?;
    }
    Ok(())
}

fn script_flush(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    if ctx.arg_count() > 3 {
        return Err(RedisError::wrong_number_of_args(b"script|flush"));
    }
    if ctx.arg_count() == 3 {
        let mode = ctx.arg_owned(2usize)?;
        let b = mode.as_bytes();
        if !ascii_eq_ci(b, b"ASYNC") && !ascii_eq_ci(b, b"SYNC") {
            return Err(RedisError::syntax(b"syntax error"));
        }
    }
    let mut guard = match script_cache().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard.clear();
    ctx.reply_simple_string(b"OK")
}

fn script_help(ctx: &mut CommandContext<'_>) -> RedisResult<()> {
    let lines: &[&[u8]] = &[
        b"SCRIPT EXISTS <sha1> [<sha1> ...]",
        b"    Return information about the existence of the scripts in the script cache.",
        b"SCRIPT FLUSH [ASYNC|SYNC]",
        b"    Flush the Lua scripts cache. Very dangerous on replicas.",
        b"SCRIPT LOAD <script>",
        b"    Load a script into the scripts cache without executing it.",
        b"HELP",
        b"    Prints this help.",
    ];
    ctx.reply_array_header(lines.len() as i64)?;
    for ln in lines {
        ctx.reply_bulk(ln)?;
    }
    Ok(())
}

/// Strict integer parse for `numkeys`. Reuses the canonical error string.
fn parse_i64(bytes: &[u8]) -> Result<i64, RedisError> {
    let s = std::str::from_utf8(bytes).map_err(|_| RedisError::not_integer())?;
    s.parse::<i64>().map_err(|_| RedisError::not_integer())
}

/// Accept any case for the input sha; return `Some` with the lowercase
/// canonical 40-byte buffer when the input is exactly 40 hex bytes.
fn normalise_sha(bytes: &[u8]) -> Option<[u8; 40]> {
    if bytes.len() != 40 {
        return None;
    }
    let mut out = [0u8; 40];
    for (i, b) in bytes.iter().enumerate() {
        let c = match *b {
            b'0'..=b'9' | b'a'..=b'f' => *b,
            b'A'..=b'F' => *b + 32,
            _ => return None,
        };
        out[i] = c;
    }
    Some(out)
}

fn ascii_eq_ci(a: &[u8], b: &[u8]) -> bool {
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

/// Compute the lowercase 40-byte SHA-1 hex digest of `data` using a
/// pure-Rust implementation. Stays inside this crate so we do not pull in
/// a hash-crate dependency for a single use site.
fn sha1_hex(data: &[u8]) -> [u8; 40] {
    let digest = sha1_digest(data);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = [0u8; 40];
    for (i, byte) in digest.iter().enumerate() {
        out[i * 2] = HEX[(byte >> 4) as usize];
        out[i * 2 + 1] = HEX[(byte & 0x0f) as usize];
    }
    out
}

/// Compute the raw 20-byte SHA-1 digest of `data`.
///
/// Direct translation of FIPS 180-4 §6.1.2; zero unsafe, no dependency.
fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let bit_len: u64 = (data.len() as u64) * 8;

    let mut padded: Vec<u8> = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for i in 0..80 {
            let (f, k) = if i < 20 {
                ((b & c) | ((!b) & d), 0x5A827999u32)
            } else if i < 40 {
                (b ^ c ^ d, 0x6ED9EBA1u32)
            } else if i < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32)
            } else {
                (b ^ c ^ d, 0xCA62C1D6u32)
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_hex_known_vectors() {
        let empty = sha1_hex(b"");
        assert_eq!(&empty, b"da39a3ee5e6b4b0d3255bfef95601890afd80709");
        let abc = sha1_hex(b"abc");
        assert_eq!(&abc, b"a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn normalise_sha_lowercases() {
        let upper = b"DA39A3EE5E6B4B0D3255BFEF95601890AFD80709";
        let n = normalise_sha(upper).unwrap();
        assert_eq!(&n, b"da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn normalise_sha_rejects_non_hex() {
        assert!(normalise_sha(b"short").is_none());
        assert!(normalise_sha(b"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_none());
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        Session 1A — EVAL / EVALSHA / SCRIPT family
//   target_crate:  redis-commands
//   confidence:    high
//   todos:         5 (EVAL_RO, script replication, SCRIPT KILL, FUNCTION,
//                    pcall traceback formatting)
//   port_notes:    1
//   unsafe_blocks: 0
//   notes:         mlua-backed Lua 5.1 runtime, per-call instance, sandboxed.
//                  Pure-Rust SHA-1; reply parser reused from redis-protocol.
// ──────────────────────────────────────────────────────────────────────────
