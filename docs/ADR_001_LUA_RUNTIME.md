# ADR 001 — Lua Runtime for EVAL / EVALSHA / SCRIPT

**Status:** Accepted (2026-05).
**Scope:** `crates/redis-commands/src/eval.rs` and the EVAL/EVALSHA/SCRIPT
family wired into `dispatch.rs`.

## Decision

Use `mlua = "0.10"` with the `lua51` and `vendored` Cargo features as the
embedded Lua runtime backing `redis.call` scripts. mlua bundles the official
PUC-Rio C Lua 5.1 source so there is no system Lua dependency.

## Alternatives considered

1. **Raw libc FFI to a system Lua.** Rejected. Calling `lua_pcall`,
   `lua_pushlstring`, `lua_newtable`, and friends through `extern "C"`
   requires `unsafe` at every call site. Our crates target a zero-`unsafe`
   budget; the per-site cost would dominate this module.
2. **Pure-Rust interpreter (e.g. `piccolo`, `rlua` minus mlua, `hlua`).**
   Rejected. Real Redis ships Lua 5.1 with the exact `string.byte` /
   `table.maxn` / `tostring` semantics scripts in the wild depend on. A
   pure-Rust interpreter would either lag the dialect (piccolo) or replace
   the dependency tree without solving the unsafe issue (hlua). Wire-diff
   parity matters more than dependency purity here.
3. **`lua-rs-port` (the sibling Rust port of Lua 5.1 we maintain).**
   Deferred. The port is incomplete — `string.format`, the `os` library,
   coroutine state machine, and the bytecode loader are still in flight.
   Once that crate reaches feature parity for the subset Redis scripts use,
   we can swap it in behind the same dispatch boundary without touching
   call sites.

## Consequences

- **Binary size:** roughly +150–300 KB for the embedded Lua C
  implementation. Acceptable for a server binary.
- **Build dependency:** adds a C compile step (cc-rs invokes the host
  toolchain). Vendoring the C source keeps this hermetic — no system
  package is required on Linux/macOS targets.
- **`unsafe` containment:** mlua's internal `unsafe` lives inside the
  dependency, not our crates. `grep -rn '\bunsafe\b' crates/` continues to
  return zero hits in our own code.
- **Sandbox responsibility on our side:** mlua exposes the full Lua 5.1
  standard library by default. We must explicitly remove dangerous
  globals before user scripts run (see below).

## Reversal path

The Lua runtime is reachable only through the entrypoints
`eval_command`, `evalsha_command`, and `script_command` in
`crates/redis-commands/src/eval.rs`. Each one constructs a fresh `Lua`
instance per call; nothing about mlua leaks into the dispatch table, the
command context, or any other crate. Swapping to `lua-rs-port` (or any
other 5.1-compatible runtime) requires replacing the body of those three
functions and the script cache helper — no change to `dispatch.rs`, the
client, or the protocol layer.

## Sandbox patches we apply

Every per-call `Lua` instance has the following globals removed before
user code runs, matching real Redis behaviour:

| Removed | Why |
|--------|-----|
| `os` | Filesystem, `getenv`, `execute`. |
| `io` | Filesystem, stdout/stderr. |
| `debug` | Stack introspection, hook installation. |
| `package`, `require` | Loading arbitrary code from disk. |
| `loadfile`, `dofile` | Same. |
| `print` | Should go to the server log, not stdout; not exposed today. |

`load`, `loadstring`, and `string.dump` remain available; user scripts
that compile inline strings still work, but they cannot reach the
filesystem.

## Injected `redis` table

| Key | Behaviour |
|-----|-----------|
| `redis.call(cmd, ...)` | Re-enter `dispatch_command_name`. Convert reply bytes to a Lua value. On `-ERR`, throw a Lua error. |
| `redis.pcall(cmd, ...)` | Same, but on `-ERR` return `{err = "..."}` instead of throwing. |
| `redis.error_reply(s)` | Return `{err = s}`; serialised as `-s` on the wire. |
| `redis.status_reply(s)` | Return `{ok = s}`; serialised as `+s`. |
| `redis.sha1hex(s)` | SHA-1 hex digest of `s`. |
| `redis.replicate_commands()` | No-op stub. Returns `true`. |

`KEYS` and `ARGV` are injected as 1-indexed Lua tables.

## Open follow-ups

- `EVAL_RO` read-only variant.
- Replication of script effects via `propagate` (depends on the
  replication layer landing).
- `SCRIPT KILL` for cooperative slow-script termination.
- `FUNCTION` family (Lua functions registered server-side).
- Pcall traceback formatting parity with Redis 7.x.
