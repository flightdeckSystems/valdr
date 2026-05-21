# Runtime Ownership Plan

Status: architecture decision after the first Redis performance loop, 2026-05-21.

This doc captures the "option 5" performance question: whether to move beyond
local hot-path patches and change who owns sockets, clients, and databases.

## Decision

Do not implement the runtime-ownership rewrite as a same-day Redis patch.

The first four no-regret optimizations already landed:

- batch replies per socket read;
- drain the query buffer once per read batch;
- direct-write ordinary request/reply traffic;
- batch client-info snapshots, reuse argv storage, use monotonic timing, and
  hold the DB0 lock across safe read batches.

Those moved deep-pipeline GET from roughly 221k req/s to roughly 957k req/s.
That is a real improvement, and it also exposes the remaining architecture
gap: valkey-rs still has blocking per-client threads sharing
`Arc<Mutex<RedisDb>>`, while upstream Valkey drains many sockets and executes
commands from a tight event loop.

The next step is not another small hot-path edit. It is a runtime ownership
rewrite. Doing that casually would either fork the semantics or turn the
benchmark into a lie.

## Current Runtime Shape

```text
TcpListener::incoming()
  accept one socket
  spawn read/dispatch thread
    Client owns query_buf, argv, reply_buf, selected db, pub/sub state
    parse one read batch
    lock Arc<Mutex<RedisDb>>
    build CommandContext { &mut Client, &mut RedisDb, Arc<RedisServer>, pubsub }
    dispatch command
    write ordinary replies directly
  spawn writer thread
    used by pub/sub, blocked-list wakeups, replica pushes

background threads:
  active expire
  blocked-key timeout
  replication/AOF/BGSAVE helpers
  client-info, pub/sub, ACL, slowlog globals behind Arc<Mutex<_>>
```

This shape was good for compatibility bring-up. It lets agents port commands
without understanding an event loop. It is not the production Valkey shape.

The main blocking points are visible in the code:

- `redis-core/src/databases.rs` stores each DB as `Arc<Mutex<RedisDb>>`.
- `redis-core/src/command_context.rs` gives every command a mutable client and
  mutable DB.
- `redis-server/src/main.rs` accepts a socket, spawns a client thread, spawns a
  writer thread, and locks the DB around dispatch.
- pub/sub, blocked keys, replication, and client metadata assume cross-thread
  communication through registries and channels.

## Why Not Patch It Quickly

### A command fast path is dishonest

We could special-case `PING`, `GET`, `SET`, and `INCR` in the server loop and
bypass `dispatch`, ACL checks, slowlog, AOF, replication, maxmemory, scripts,
and transaction semantics. The benchmark would improve. The port would be
worse.

That kind of patch is exactly what the harness should prevent: it optimizes a
scoreboard by stepping around the compatibility surface.

### `RwLock` is not the real fix

Changing `Arc<Mutex<RedisDb>>` to `Arc<RwLock<RedisDb>>` sounds attractive for
GET-heavy workloads, but command execution today is typed as `&mut RedisDb`.
Read-only command dispatch would need a real read-only command context,
generated command metadata wired into the dispatcher, and careful handling for
commands that look read-only but expire keys, touch LRU/LFU metadata, wake
blocked clients, or update client/server statistics.

That can be a useful subproject, but it is not the runtime ownership rewrite.

### Sharding is not automatically Redis-compatible

Key-range sharding removes one global DB lock, but Redis semantics make it
expensive:

- multi-key commands can cross shards;
- `MULTI` / `WATCH` / `EXEC` want atomic behavior across selected keys;
- `SELECT` changes per-client DB, not a shard;
- blocking list commands park clients and wake them from write paths;
- Lua scripts and replication want ordered, single-threaded command effects.

Sharding is a product decision, not a translation cleanup.

## Viable Designs

### A. Faithful event-loop runtime

One runtime thread owns normal clients and the selected DBs. Sockets are
nonblocking. The loop polls readiness, drains request bytes, dispatches command
batches, and flushes replies. Background systems send events into the owner
loop rather than taking DB locks directly.

```text
poller/kqueue/epoll/mio
  ready client sockets
  timer events
  background events
        |
        v
RuntimeOwner
  Vec<Client>
  Vec<RedisDb>
  PubSubRegistry
  BlockedKeysIndex
  Slowlog/metrics
        |
        v
CommandContext { &mut RuntimeOwner, client_id }
```

Pros:

- closest to C Valkey's `ae.c` model;
- removes the DB mutex from the hot path;
- makes pipelined tiny commands much more competitive;
- gives one coherent place for timers, active expire, blocked wakeups, pub/sub,
  and replication events.

Cons:

- forces a real `CommandContext` redesign;
- background helpers must become event senders, not direct DB mutators;
- TLS, pub/sub, blocking commands, replication, and persistence must be
  rechecked under the new owner model;
- this is a milestone, not a hotfix.

Recommendation: this is the production direction if valkey-rs keeps going.

### B. Shard-owned workers

Network threads parse requests and route command batches to shard workers. Each
worker owns a DB partition. Replies flow back to the client writer.

Pros:

- can scale beyond upstream's single command thread for independent keys;
- maps to modern multicore cache-server designs.

Cons:

- Redis compatibility gets hard around transactions, scripts, multi-key ops,
  blocking commands, and replication ordering;
- requires a command-effect protocol instead of direct `&mut Client` mutation;
- not a faithful port of upstream Valkey.

Recommendation: not the first production rewrite. Revisit after a faithful
owner loop exists and after the compatibility envelope is stable.

### C. Tokio with shared DB locks

Move socket handling to async tasks but keep `Arc<Mutex<RedisDb>>`.

Pros:

- reduces thread count;
- can handle many idle clients well;
- ecosystem support is strong.

Cons:

- does not remove the command-path DB lock;
- async locks around CPU-bound command execution can worsen tail latency;
- adds a runtime dependency without solving the benchmark cliff.

Recommendation: useful for connection scalability, not the core #5 fix.

### D. Benchmark-only owned-DB mode

Add an environment flag that runs only `PING` / `GET` / `SET` / `INCR` through a
small single-thread owner loop.

Pros:

- quickly estimates the event-loop ceiling.

Cons:

- creates a second server with a smaller semantic surface;
- risks publishing numbers from a mode that is not the product;
- teaches agents that benchmark-specific shortcuts are acceptable.

Recommendation: reject for public numbers. A private scratch experiment is fine,
but it should not land as the default benchmark path.

## Proposed Harness Packet For A Real Rewrite

If we choose to spend on #5 later, create a packet family rather than one giant
agent task:

```json
{"id":"runtime-owner-0-design","role":"architect","selector":"manual"}
{"id":"runtime-owner-1-client-table","role":"translator","selector":"manual"}
{"id":"runtime-owner-2-nonblocking-poller","role":"translator","selector":"manual"}
{"id":"runtime-owner-3-command-context","role":"translator","selector":"manual"}
{"id":"runtime-owner-4-background-events","role":"translator","selector":"manual"}
{"id":"runtime-owner-5-pubsub-blocking-replication","role":"translator","selector":"manual"}
{"id":"runtime-owner-6-bench-and-soak","role":"runner","selector":"nightly"}
```

Required gates:

- existing smoke oracle: `21 / 21`;
- RDB bidirectional oracle: no regressions;
- official TCL surveyed files: no regressions;
- profile matrix: update the benchmark table every iteration;
- one 30-minute soak before claiming production performance;
- no command-specific benchmark bypasses.

The packet should not be "make Redis faster." It should be "move command
execution ownership to one runtime owner while preserving the command surface."

## What This Means For nginx

The Redis experiment is directly useful for nginx, but mostly as a warning.

For nginx, runtime ownership is not a late performance cleanup. It is the
architecture:

- which event loop owns sockets;
- which worker owns request state;
- where timers live;
- how sendfile, TLS, keepalive, upstream proxying, and graceful shutdown feed
  events back into the loop;
- which global/shared structures are intentionally shared vs owned.

The nginx port should not begin with a blocking thread-per-connection skeleton
and then try to optimize backward. It should start with an explicit runtime
owner model, then generate packets around that model:

```text
runtime owner
  -> accept/listen sockets
  -> connection table
  -> request parser state
  -> timer wheel
  -> file/sendfile path
  -> upstream/proxy path
  -> graceful reload/shutdown path
```

Benchmarks should be present from the first useful server loop, not after
conformance is already green. The lesson from valkey-rs is that conformance can
look excellent while the runtime shape still caps throughput.

## Stop Condition For This Redis Loop

Call the Redis performance loop complete for now.

What we learned:

- the harness can track conformance and performance in the same repo;
- performance packets work when they name a subsystem boundary;
- small no-regret patches can produce large wins;
- the remaining gap is architectural, not incidental Rust overhead;
- the nginx run should make runtime ownership a first-class architect packet
  before translator agents start filling in large surfaces.

The honest next Redis milestone is not another micro-iteration. It is an
explicit runtime-owner project with its own packet graph and budget.
