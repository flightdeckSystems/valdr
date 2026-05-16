# redis-rs-port

Port of [Valkey](https://github.com/valkey-io/valkey) (BSD-3-Clause Redis
fork) from C to safe Rust, using the AI-driven porting harness at
sibling repo [`../port-harness/`](../port-harness/).

Status: **scaffolding.** Crates exist as empty skeletons; no translation
yet. See `docs/REDIS_PORT_HARNESS_SPEC.md` in `../lua-rs-port/` for the
phase plan and oracle design that this project implements.

## Why Valkey, not Redis

Redis 8+ is tri-licensed AGPL/SSPL/RSAL — not freely redistributable in
a Rust port. Valkey is the BSD-3 fork from Redis 7.2.4 (March 2024),
actively maintained by the Linux Foundation. For a port intended as
distributable source, Valkey is the right upstream.

## Layout

```
redis-rs-port/
├── README.md
├── Cargo.toml                      <- workspace root
├── PORTING.md                      <- agent-facing translation spec (TODO)
├── reference/
│   └── valkey/                     <- upstream C source (pinned)
├── harness/
│   ├── source.toml                 <- pinned commit + build commands
│   ├── type-vocabulary.tsv         <- 13 cross-cutting types per spec
│   ├── forbidden-patterns.sh       <- chassis-consumed banned patterns
│   ├── unsafe-budgets.toml         <- per-crate unsafe ceilings
│   ├── check_type_vocabulary.py    <- scanner (TODO; copied/adapted from lua)
│   └── oracle/                     <- wire-diff, tcl-external, etc. (TODO)
├── .claude/
│   ├── settings.json
│   └── hooks/                      <- 7 thin wrappers to ../port-harness/hooks/
└── crates/
    ├── redis-types/                <- ByteString, RespValue, RedisError, ...
    ├── redis-protocol/             <- RESP2/RESP3 parser + serializer
    ├── redis-core/                 <- RedisServer, Client, RedisDb, RedisObject
    ├── redis-commands/             <- generated registry + impls
    └── redis-server/               <- the binary
```

Deferred for later phases per spec: `redis-ds`, `redis-persist`,
`redis-repl`, `redis-cluster`, `redis-sentinel`, `redis-modules`,
`redis-cli`, `redis-benchmark`.

## Pilot scope

Per `REDIS_PORT_HARNESS_SPEC.md §First Pilot`:

1. RESP2/RESP3 frame model + parser/serializer.
2. Minimal single-threaded TCP loop.
3. PING, ECHO, HELLO, COMMAND enough for protocol tests.
4. SET / GET / DEL / EXISTS / INCR.
5. Pass `unit/protocol` Tcl tests in external mode against Rust server.
6. Wire-diff against Valkey C server on a smoke suite.

## Dependencies

- Sibling chassis at `../port-harness/` provides hook logic and (later)
  orchestration scripts.
- `reference/valkey/` is git-cloned, not vendored; rebuild required for
  the C oracle.
