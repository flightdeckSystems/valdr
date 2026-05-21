# Redis / Valkey System Deep Dive

Status date: 2026-05-18.

This document is a current map of the Redis/Valkey Rust port: what Redis and
Valkey are, how the original system is structured, how this workspace models
it, what we have built so far, and how to reason about the next phases.

The project is using Valkey as the reference codebase while aiming at the
Redis-compatible server surface. In practice, most users experience this as
"Redis protocol and Redis commands"; the upstream C source in this repo is
Valkey.

## One-Screen Model

Redis/Valkey is an in-memory data-structure server:

```text
TCP client
  -> RESP parser
  -> argv command vector
  -> command lookup / auth / transaction / pubsub gates
  -> command handler
  -> RedisDb keyspace mutation/read
  -> RESP reply bytes
  -> TCP client
```

The Rust workspace splits that into crates:

```text
redis-server
  -> redis-commands
       -> redis-core
            -> redis-types
       -> redis-protocol
  -> redis-core
       -> redis-types
  -> redis-ds
```

The current server is a blocking thread-per-connection implementation, not a
faithful `ae.c` event-loop port. That is acceptable for the current goal:
compatibility and harness-driven progress before we decide whether a production
runtime should keep the blocking shape, adopt Tokio, or port the C event loop
more directly.

## Redis vs Valkey

Redis is the original project and protocol/ecosystem name most operators know.
Valkey is the Linux Foundation fork created after Redis Ltd changed Redis's
license. The command surface, wire protocol, data types, and test suite remain
close enough that Valkey is an excellent open reference for a Redis-compatible
implementation.

For this port:

- "Redis" usually means the protocol, commands, behavior, and client
  compatibility target.
- "Valkey" usually means the C reference implementation and TCL suite we are
  porting against.
- The product shape can be "Redis-compatible Rust Valkey" without needing to
  exactly clone every Redis Ltd feature, especially proprietary/license-adjacent
  work.

## How Redis / Valkey Works

### Protocol

Clients speak RESP. The important RESP2 frame types are:

- simple strings: `+OK\r\n`
- errors: `-ERR ...\r\n`
- integers: `:123\r\n`
- bulk strings: `$3\r\nfoo\r\n`
- arrays: `*2\r\n$3\r\nGET\r\n$3\r\nkey\r\n`

Redis commands are just arrays where `argv[0]` is the command name and the rest
are byte-string arguments. Inline command parsing also exists for telnet-like
use.

In this port:

- `redis-protocol/src/frame.rs` defines RESP frames and encoding.
- `redis-protocol/src/parser.rs` and `request.rs` parse request bytes.
- `redis-server/src/main.rs` holds the live connection read loop and calls
  `parse_inline_or_multibulk`.

Everything is bytes. Command names are ASCII case-insensitive; keys and values
are arbitrary byte strings.

### Server State

Original Redis has a giant global `server` struct. It carries databases,
clients, config, persistence state, replication state, metrics, command table,
event loop handles, pub/sub, eviction state, and many counters.

The Rust port is converging on:

```text
Arc<RedisServer>
  live_config: Arc<LiveConfig>
  dbs: Vec<RedisDb>
  eviction_pool: Mutex<EvictionPool>
  dirty counters / command-time snapshots / shutdown flags
  startup config / listeners / future persistence state

Arc<Mutex<RedisDb>>
  current live DB used by redis-server today

CommandContext
  &mut Client
  &mut RedisDb
  Arc<RedisServer>
  optional Arc<Mutex<PubSubRegistry>>
```

The recent important change is that `RedisServer` is now actually shared into
command handlers through `CommandContext::with_server`. Earlier designs had
too many local config maps and fake server handles. The current spine is the
right shape: one live server, atomics for hot config reads, explicit context
for commands.

### Keyspace

Each Redis database maps keys to objects plus optional expiration metadata.
Conceptually:

```text
RedisDb
  dict: key -> RedisObject
  expires: key -> expire_at_ms
  watched keys / blocking keys / dirty state / metadata
```

The C implementation uses custom dictionaries, object encodings, and special
memory accounting. The Rust port currently uses safer, more direct structures
inside `redis-core/src/db.rs` while `redis-ds` carries ports of lower-level
data structures for later fidelity/performance work.

The keyspace has to support:

- lookup for reads;
- lookup for writes;
- lazy expiration on access;
- active expiration in the background;
- deletion/unlink semantics;
- WATCH/MULTI invalidation;
- keyspace notification events;
- eviction sampling;
- RDB persistence later.

### Objects and Encodings

Redis values are typed objects:

- string
- list
- hash
- set
- sorted set
- stream
- module values

For performance, each type has multiple encodings. Examples:

- small integer-like strings can be integer encoded;
- small hashes and zsets use listpack;
- sets of integers can use intset;
- large lists use quicklist/listpack nodes;
- sorted sets use skiplist plus dictionary;
- streams use radix tree/listpack structures.

The Rust port has two layers:

- `redis-core/src/object.rs`: the object model used by command handlers.
- `redis-ds`: translated lower-level data structures such as dict, intset,
  listpack, quicklist, rax, stream, ziplist/zipmap, zskiplist.

The current implementation is not yet a perfect encoding-by-encoding clone.
That is fine at this stage as long as wire behavior and persistence plans are
honest about the gap. For RDB compatibility, object encodings will matter much
more because bytes on disk encode type and encoding choices.

### Command Dispatch

The dispatch path is:

```text
Client.query_buf
  -> parse request into Vec<RedisString>
  -> client.set_args(argv)
  -> CommandContext::with_server(...)
  -> redis_commands::dispatch(ctx)
  -> HANDLERS lookup by ASCII-insensitive command name
  -> handler(ctx)
  -> handler appends RespFrame bytes to client.reply_buf
  -> server flushes reply_buf through writer thread
```

`redis-commands/src/generated.rs` contains command metadata generated from the
upstream command table. `redis-commands/src/dispatch.rs` contains the static
handler table. As of this snapshot, there are 191 `DispatchEntry` handlers in
the table.

`dispatch` also enforces:

- MULTI queueing rules;
- pub/sub subscribed-mode command restrictions;
- `requirepass` authentication gate using generated `NO_AUTH` command flags;
- slowlog timing around handler execution.

This is one of the best examples of the harness pattern: static-generate the
big declarative command table, then hand/agent-port handler behavior.

### Connections

The live server in `redis-server/src/main.rs` is intentionally simple:

```text
TcpListener accept loop
  maxclients check
  spawn one client thread
    clone stream
    spawn writer thread
    register sender in PubSubRegistry
    read bytes into query_buf
    parse commands
    lock shared RedisDb
    dispatch
    flush replies to writer mpsc
```

This is not how production Redis is architected. C Redis/Valkey uses a single
threaded event loop for command execution, optional I/O threads, and careful
latency control. The Rust port's blocking shape is good for bringing up the
wire surface and tests quickly.

The risk is that background threads plus a big `Mutex<RedisDb>` can hide timing
bugs and make some Redis semantics less natural. Before a serious production
claim, we should decide whether to:

- keep thread-per-connection and optimize/guard it;
- move to Tokio;
- port the event-loop model more faithfully;
- build a hybrid where command execution is serialized but I/O is async.

## Workspace Map

### `redis-types`

Shared primitives:

- `RedisString`: byte-string type for keys, values, command arguments, and
  protocol payloads.
- `RedisError`: structured error surface that can render RESP errors.
- `RedisResult`.

This crate should stay small and dependency-light. Anything here is part of the
cross-crate vocabulary.

### `redis-protocol`

RESP frame parsing and encoding:

- frame definitions;
- parser;
- request parsing helpers;
- RESP2 encoding.

The protocol crate should not know about server state or command semantics. It
turns bytes into frames/argv and frames into bytes.

### `redis-ds`

Lower-level Redis data structures:

- adlist
- dict / hashtable / kvstore
- intset
- listpack
- quicklist
- rax
- stream
- ziplist / zipmap
- zskiplist

This crate is a staging ground for data-structure fidelity. Some command
handlers currently use simpler Rust collections. Over time, hot paths and RDB
encoding fidelity may pull more of `redis-ds` into the live object model.

The architectural issue to watch: do not let a command handler invent a local
mini-version of a canonical data structure if `redis-ds` owns it. That is the
same type-drift problem seen in the Lua port.

### `redis-core`

Server internals and shared runtime systems:

- `client.rs`: client state, argv, reply buffer, auth flag, db index, pub/sub
  flags, MULTI state.
- `command_context.rs`: command-handler API over client/db/server/pubsub.
- `db.rs`: keyspace operations and generic key commands.
- `object.rs`: Redis object model and live encoding config handle.
- `server.rs`: `RedisServer`, global counters, live config, eviction pool.
- `live_config.rs`: runtime-tunable config knobs using atomics/mutexes.
- `expire.rs`: TTL and active expiration machinery.
- `evict.rs`: maxmemory/eviction port skeleton.
- `notify.rs`: notification constants and parser/stringifier, older core
  notify function.
- `pubsub_registry.rs`: shared registry for channels, patterns, and client
  senders.
- `metrics.rs`, `latency.rs`, `commandlog.rs`, `memory.rs`, `networking.rs`,
  and many translated supporting modules.

`redis-core` is where most architectural decisions belong. `redis-commands`
should call into it rather than duplicating global server behavior.

### `redis-commands`

Command implementations and dispatch:

- connection/server commands: PING, ECHO, HELLO, CLIENT, CONFIG, AUTH, ACL,
  MEMORY, COMMAND, TIME, DEBUG, RESET, QUIT.
- strings.
- lists, including blocking list operations.
- hashes.
- sets.
- sorted sets.
- streams.
- geo.
- HyperLogLog.
- bit operations.
- transactions.
- pub/sub.
- sort.
- info.
- slowlog/latency.
- generated command metadata.

This crate is where the port has made the most visible progress. The broad
command surface is present, but depth varies by family. Some commands are
close enough for TCL progress; others are compatibility stubs with the right
reply shape.

### `redis-server`

Executable server:

- CLI/config parsing;
- `TcpListener`;
- connection thread spawn;
- writer thread spawn;
- shared `RedisDb`;
- shared `PubSubRegistry`;
- shared `Arc<RedisServer>`;
- `maxclients` enforcement;
- active expiration background thread;
- blocked command timeout thread.

`redis-server/src/server.rs` is a translated server-support module, while
`redis-server/src/main.rs` is the current live harness/server entrypoint. Keep
that distinction clear: not every translated server function is on the live
request path yet.

## Built Systems So Far

### Live Config Spine

This is one of the most important recent improvements.

`redis-core/src/live_config.rs` is now the source of truth for operational
settings that commands/background threads must read at runtime:

- `maxmemory`
- `maxmemory-policy`
- `maxclients`
- `requirepass`
- `notify-keyspace-events`
- slowlog threshold and max length
- active expire effort
- `hz`
- listpack/intset encoding thresholds

`redis-server` constructs `Arc<LiveConfig>`, installs it into object/command
global handles where needed, and puts it on `Arc<RedisServer>`.

`CONFIG GET` reads dynamic values from `LiveConfig`. `CONFIG SET` updates many
of them live, including:

- maxmemory
- maxmemory-policy
- maxclients
- requirepass
- notify-keyspace-events
- hash/list/set/zset encoding thresholds
- slowlog-log-slower-than
- slowlog-max-len
- active-expire-effort
- hz

This is the correct shape for Def 3.2. The remaining work is making every
behavioral subsystem actually consult those fields.

### AUTH and Minimal ACL

Implemented:

- `AUTH password`
- `AUTH username password` with only the default user conceptually supported
- `requirepass` stored in `LiveConfig`
- new clients start authenticated when no password is configured
- dispatch rejects non-`NO_AUTH` commands while unauthenticated
- `ACL WHOAMI`
- `ACL LIST`
- `ACL GETUSER default`
- `ACL CAT`
- `ACL HELP`
- `ACL SETUSER` rejected rather than silently accepted

This is intentionally not full Redis ACL. There are no real users,
categories, key patterns, channel patterns, or hashed password lists. For Def 3
cache use, that is the right scope.

### Pub/Sub

Implemented shape:

- `PubSubRegistry` shared behind `Arc<Mutex<_>>`;
- each connection registers a writer-thread sender;
- SUBSCRIBE/UNSUBSCRIBE/PSUBSCRIBE/PUNSUBSCRIBE/PUBLISH/PUBSUB handlers;
- subscribed-mode command gating in dispatch;
- keyspace notification publishing helper on `CommandContext`.

Keyspace notifications are partially wired:

- config parser/stringifier exists;
- `LiveConfig` stores the notification mask;
- `CommandContext::notify_keyspace_event` publishes the two Redis channel
  families through `PubSubRegistry`;
- list command call sites are substantially wired;
- many other write command families still have TODOs or no event calls.

The older `redis-core/src/notify.rs::notify_keyspace_event` still has a
hard-coded zero placeholder and circular-dependency TODOs. Treat
`CommandContext::notify_keyspace_event` as the live path for command handlers.

### Expiration

Implemented/partially implemented:

- TTL-setting commands and expiration metadata;
- lazy expiration on key access in core DB paths;
- active expiration config (`active-expire-effort`, `hz`);
- background active-expire thread spawned by `redis-server`;
- expire-related command surface has made enough progress for keyops tests.

Remaining risks:

- exact active-expire cycle behavior is not a faithful event-loop port;
- timing-sensitive TCL tests can be flaky;
- multi-DB routing is still simplified by the live server using one shared DB;
- notification hooks for expire/delete need comprehensive coverage.

### Blocking List Operations

The server has a global blocked-keys index and a timeout scanner:

- BLPOP/BRPOP/BLMOVE/BRPOPLPUSH/BLMPOP handlers can park clients;
- a background `blocked-timeout` thread wakes timed-out waiters;
- writer-thread senders allow replies to be delivered out-of-band.

This is a pragmatic adaptation to the blocking thread-per-connection server.
It is not the same shape as C Redis's event loop, but it gives a useful
compatibility surface for tests.

### Observability

Implemented/partially implemented:

- INFO command with sections and dynamic metrics;
- `used_memory_estimated:true` style estimator signaling;
- server metrics for connections, rejected connections, command count, active
  main-thread time, etc.;
- SLOWLOG command family;
- LATENCY command surface tied to slowlog/latency support;
- handler timing in dispatch;
- maxclients counter/rejection at accept time.

The memory story is intentionally estimated, not allocator-exact. That is the
right Def 3 tradeoff unless/until we commit to a custom global allocator or
jemalloc-like instrumentation.

### Command Families

Broadly present:

- connection basics;
- generic key operations;
- string operations;
- list operations, including blocking variants;
- hash operations;
- set operations;
- sorted set operations;
- streams and XINFO surface;
- geo commands;
- HyperLogLog;
- bit operations;
- transactions;
- pub/sub;
- sort;
- info/config/memory/debug/time/command;
- slowlog/latency.

Depth varies. Some handlers are production-shaped; others exist to satisfy
client/test expectations with a subset reply.

## Def 3 Status

`docs/PATH_TO_DEF3.md` defines a "prod-safe cache" target:

- RDB persistence;
- AUTH/requirepass;
- maxmemory enforcement with noeviction and allkeys-lru;
- active expiration;
- keyspace notifications;
- CONFIG SET behavioral hooks;
- slowlog/latency;
- maxclients.

Current status against that target:

```text
maxclients              built at accept time
LiveConfig spine        built
CONFIG SET hooks        substantially built
AUTH/minimal ACL        built
slowlog/latency         substantially built
active expiration       partially built and running
keyspace notifications  partially built, call-site coverage incomplete
maxmemory enforcement   not yet real
allkeys-lru eviction    not yet real
RDB persistence         not yet built
```

The key thing: Def 3.2 no longer needs to start by inventing a config spine.
That part exists. Def 3.2 should now focus on making memory/eviction and
notification call sites consume the spine.

## Maxmemory and Eviction

The current `evict.rs` is a serious translated skeleton, not a finished
feature.

What exists:

- eviction policy enum;
- eviction pool data structure;
- pool insertion logic;
- memory-state API shape;
- time-event scaffolding;
- main `perform_evictions` structure;
- `RedisServer.eviction_pool`;
- `LiveConfig.maxmemory` and `LiveConfig.maxmemory_policy`.

What is not yet real:

- actual allocator or estimator integration in `get_maxmemory_state`;
- actual use of `LiveConfig.maxmemory_policy` in `perform_evictions`;
- sampling keys from live DB dictionaries;
- deleting chosen keys and measuring freed memory;
- invoking eviction before write commands;
- keyspace notification for evicted keys;
- complete metrics.

The pragmatic next design remains the estimator:

```text
used_memory_estimate =
  dict_entry_count * fixed_overhead
  + key byte lengths
  + value byte lengths / approximate nested structure sizes
```

Then:

```text
if maxmemory == 0:
  allow write
else if estimated_used <= maxmemory:
  allow write
else if policy == noeviction:
  reject write with OOM command not allowed
else if policy == allkeys-lru:
  sample keys, evict oldest touched keys until estimate <= maxmemory
```

This is not as exact as C Redis's `zmalloc_used_memory`, but it is operator
actionable and much cheaper. Continue labeling it as estimated in INFO.

## Persistence / RDB

Not built yet.

For Def 3, RDB is the right persistence target. AOF should stay out of scope
unless a concrete user needs it.

Why RDB matters:

- many TCL tests use restart/reload flows;
- operators expect cache warm-start or snapshotting;
- RDB is bounded and testable with cross-load oracles;
- AOF is a larger write-rewrite-fsync subsystem with less value for a
  read-mostly cache milestone.

Recommended shape:

```text
new crate: redis-persist
  rio reader/writer
  crc64
  rdb constants generated from rdb.h
  RDB v12 encode/decode for the object types we actually support

server startup:
  if dump.rdb exists:
    load before binding listener

commands:
  SAVE / BGSAVE minimal support
  LASTSAVE already has command surface
```

Testing should use four oracle modes:

- C Valkey saves, Rust loads;
- Rust saves, Rust loads;
- Rust saves, C Valkey loads;
- wire-diff after reload.

## Harness and Testing

The harness stack is broader than `cargo test`:

- workspace compile checks;
- direct TCP smoke tests;
- Valkey/Redis CLI compatibility checks;
- TCL suite triage documents;
- wire-diff oracle planning;
- dashboards for passing/failing TCL subsets;
- agent loops with bounded scope and regression aborts.

Important docs:

- `docs/PATH_TO_DEF3.md`: current milestone plan.
- `docs/TCL_ORACLE_PLAN.md`: oracle approach.
- `docs/TCL_DASHBOARD.md`: TCL status.
- `docs/HARNESS_LEARNINGS.md`: lessons from the harness.
- `docs/TCL_TRIAGE*.md`: command-family triage.

The right testing philosophy is:

```text
cargo check proves Rust shape
cargo test proves local invariants
TCP smoke proves the executable wires together
TCL tests prove user-visible Redis compatibility
wire-diff oracle proves byte-for-byte behavior where required
restart/load tests prove persistence
```

Do not let agents optimize for only one layer.

## Static Generation Opportunities

The best surfaces for static generation are declarative C tables:

- command registry: already generated into `redis-commands/src/generated.rs`;
- config schema from `config.c`;
- RDB type/opcode constants from `rdb.h`;
- error strings and command flags where upstream tables are authoritative;
- maybe INFO sections/field lists once the live metrics model stabilizes.

Static generation is especially valuable because agents are bad at maintaining
large copied tables under changing scope. The harness should regenerate and
diff these artifacts rather than ask agents to hand-edit them.

## How To Understand Failures

Classify failures before dispatching agents:

1. **Protocol failure.**
   RESP parse/encode shape wrong. Usually local to `redis-protocol` or reply
   construction.

2. **Command arity/error-surface failure.**
   Wrong number of args, wrong error string, wrong null-vs-empty reply. Often
   cheap and highly testable.

3. **Command semantic failure.**
   Handler behavior differs from Valkey. Good focused agent target if the
   subsystem exists.

4. **State-spine failure.**
   A handler reads a stale local config instead of `LiveConfig`, or uses a
   scratch server instead of `ctx.server()`. This is architectural.

5. **Timing/concurrency failure.**
   Blocking ops, pub/sub, active expire, timeouts, maxclients. Requires care
   because the Rust server runtime differs from C Valkey.

6. **Data-structure fidelity failure.**
   Encoding, ordering, sampling, stream/listpack/skiplist detail. Decide
   whether Def 3 needs exact fidelity or only wire behavior.

7. **Persistence failure.**
   RDB byte mismatch or reload mismatch. Needs oracle-driven diagnosis.

8. **Harness failure.**
   TCL assumptions, port conflicts, stuck subprocesses, flaky timing. Fix the
   harness or quarantine before burning agent rounds.

## Near-Term Engineering Priorities

1. Finish maxmemory enforcement using the estimator. Wire it into the write
   path before commands that may increase memory.

2. Make `perform_evictions` read `LiveConfig.maxmemory_policy` and implement
   `noeviction` plus `allkeys-lru` only.

3. Add `RedisDb::approximate_memory()` and expose memory estimate in INFO
   consistently.

4. Complete keyspace notification call-site coverage for strings, generic key
   ops, sets, hashes, zsets, streams, expire, and eviction.

5. Reconcile `redis-core/src/notify.rs` with the live `CommandContext`
   notification helper so future agents do not wire the dead path.

6. Keep AUTH minimal. Do not accidentally start a full ACL port inside Def 3.

7. Start RDB as its own bounded milestone with oracle tests first.

8. Decide the production runtime direction before making performance claims.
   Thread-per-connection is useful for progress but not obviously the final
   architecture. The post-benchmark decision is captured in
   [`RUNTIME_OWNERSHIP_PLAN.md`](RUNTIME_OWNERSHIP_PLAN.md): the remaining
   simple-command gap is a runtime-owner/event-loop problem, not a small
   per-command Rust hot-path bug.

## Product-Oriented Reading

For a real "safe critical infrastructure" story, the sellable unit is not just
"Redis in Rust." It is:

```text
Redis-compatible cache/server
  with a public compatibility matrix
  with memory safety story and unsafe budget
  with CVE/security response process
  with differential tests against Valkey
  with reproducible fuzz/property tests
  with narrow, documented non-goals
```

Open source is likely mandatory. The paid product would be around:

- hardened builds;
- support/SLA;
- security advisories and backports;
- regulated deployment docs;
- compatibility certification;
- migration tooling;
- hosted test reports against customer workloads;
- performance tuning and production support.

The engineering prerequisite is brutal clarity about scope. A Def 3 cache that
honestly supports strings/hashes/sets/lists/zsets/pubsub/auth/maxmemory/RDB is
credible. A half-implemented "full Redis replacement" is not.

## Long-Term Shape

The credible end state is:

```text
redis-types
  byte strings and errors only

redis-protocol
  RESP parser/encoder, protocol-level fuzzing

redis-ds
  canonical data structures where fidelity/perf matters

redis-core
  one RedisServer, one live config spine, DB/object/expire/evict/pubsub/persist

redis-commands
  generated metadata plus handlers, no shadow global state

redis-server
  chosen runtime architecture, integration binary, production config

redis-persist
  RDB load/save with cross-implementation oracles

harness
  TCL, wire diff, restart tests, fuzz/property tests, regression aborts,
  static-generated tables, architectural guards
```

The port should be harness-driven. Agents are useful when the target behavior
is crisp, the write scope is bounded, and regressions are automatically
detected. The human job is to choose the architecture, define the invariants,
and make it impossible for a compiling local shortcut to become a silent
workspace-wide defect.
