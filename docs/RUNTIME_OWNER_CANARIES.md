# Runtime Owner Canary Corpus

Status: added by `runtime-owner-1-canary-corpus` on 2026-05-22.

Companion doc for `harness/oracle/corpus/21-runtime-owner-canaries.txt`. It
explains *what each section guards*, *why each item is on the critical path
under the planned runtime-owner migration*, and *what is deliberately NOT
covered here* so future packets do not accidentally redraw the boundary.

The binding decisions this corpus enforces are in
`harness/architecture/decisions/runtime-ownership.md` ("Subsystem Ownership
Boundary" and "Non-Goals"). This doc adds the test-side mapping.

## Scope

| Property                                                                   | Covered here | Where the missing case lives                |
|----------------------------------------------------------------------------|--------------|----------------------------------------------|
| Transaction queuing + ordered EXEC results on one client                   | yes (B, H)   | —                                            |
| Per-command runtime error inside EXEC without batch abort                  | yes (C)      | —                                            |
| WATCH / UNWATCH / DISCARD lifecycle clears state on the calling client     | yes (D, E)   | —                                            |
| WATCH invalidation by *another* client (CAS-fail EXEC)                     | no           | needs multi-connection runner (TODO)         |
| Self-modification of a WATCHed key does NOT invalidate EXEC                | yes (F)      | —                                            |
| EXEC/DISCARD outside MULTI is an error; UNWATCH outside MULTI is a no-op   | yes (G)      | —                                            |
| Lazy expiration on direct GET/EXISTS/TTL                                   | yes (I)      | —                                            |
| Lazy expiration observed by commands queued *after* a PEXPIREAT then EXEC  | yes (J)      | —                                            |
| Expiration applied INSIDE MULTI affects subsequent queued commands         | yes (K)      | —                                            |
| Pub/sub introspection commands do not disturb ordinary SET/GET             | yes (L)      | —                                            |
| PUBLISH actually delivered to another client                               | no           | needs multi-connection runner (TODO)         |
| SUBSCRIBE / PSUBSCRIBE flow                                                | no           | needs a separate runner (puts conn in p/s)   |
| Selected-DB state persists across commands on one connection               | yes (M)      | —                                            |
| SELECT issued inside MULTI is queued and observed at EXEC time             | yes (N)      | —                                            |
| Non-hanging blocking pop variants when data is present                     | yes (O)      | —                                            |
| BLPOP / BRPOP wakeup when another client pushes data                       | no           | needs multi-connection runner (TODO)         |
| WAIT returns immediately when zero replicas are required                   | yes (P)      | —                                            |
| CLIENT SETNAME / GETNAME / RESET                                           | yes (Q)      | partial in `08-server.txt`; widened here     |
| AOF / replication ordered propagation across a batch                       | no           | exercised by the RDB+replica oracle, not here |
| Script-step (`EVAL`) replication-safe command ordering                     | no           | requires a separate `eval` canary that does not break byte_exact on error wording |

The single-connection limitation is deliberate. The current
`harness/oracle/wire-diff` driver opens exactly one socket per corpus file
and reads `len(commands)` replies from it. Adding a cross-client variant
would require a multi-connection runner; that is explicitly out of scope
for this packet (the architect decision doc lists it as a follow-up
artifact, not a target of `runtime-owner-1-canary-corpus`).

## Section-by-section rationale

The letters below match the comment markers in
`harness/oracle/corpus/21-runtime-owner-canaries.txt`.

### A. Reset state

`SELECT 0` + `FLUSHALL` so the canaries do not depend on any earlier
corpus file's residual data. `FLUSHALL` (not `FLUSHDB`) because later
sections touch DB 1 as well.

### B. Transaction ordering, queueing, post-EXEC observability

Under the runtime-owner migration, `MULTI` / `EXEC` is the highest-risk
semantic to regress: the owner loop must keep the entire EXEC batch
atomic from the caller's perspective and the queued replies must arrive
in submission order. Verifying with `INCR` / `INCRBY` interleaved with
`SET` ensures we are not silently coalescing or reordering. The
`GET` / `EXISTS` lines after `EXEC` assert the post-batch DB state was
durably committed.

### C. EXEC reports per-command runtime errors without batch abort

`LPUSH` against a string key fails at execute time with `WRONGTYPE`. The
EXEC reply must contain the error frame in array position 1 but must
NOT abort the surrounding batch — the prior `SET` must take effect and
`TYPE err_k` must read `string`. This rules out a regression where the
runtime owner short-circuits an EXEC on the first per-command failure.

### D. WATCH then UNWATCH leaves no residue

The canary distinguishes "WATCH state cleared" from "WATCH never set".
After `UNWATCH`, a subsequent MULTI/EXEC must run with no CAS guard.

### E. DISCARD clears WATCH state

`DISCARD` is responsible for unwinding both the queued command list AND
the WATCH set on the current connection. Re-issuing MULTI/EXEC right
after must succeed.

### F. Self-modification of a WATCHed key is allowed

Valkey only invalidates EXEC when *another* client modifies a WATCHed
key. The single-connection write here must therefore commit. This
encodes the invariant so a future "be safer; invalidate on any write"
change cannot land silently.

### G. EXEC / DISCARD outside MULTI; UNWATCH outside MULTI

Standalone `EXEC` and `DISCARD` are protocol errors. `UNWATCH` outside
MULTI is a benign `+OK`. These three lines fix the boundary behavior.

### H. Nested MULTI does not abort the outer transaction

`MULTI` inside `MULTI` returns an error string, but the outer batch
must keep accepting queued commands. `EXEC` then runs whatever did
queue. If a future packet decides to set `CLIENT_DIRTY_EXEC` on nested
MULTI, this canary will catch the wire incompatibility.

### I. Lazy expiration on direct access

A past `PEXPIREAT` (timestamp `1`) means the key is logically expired.
The next access (`GET`, `EXISTS`, `TTL`, `PTTL`) must report it as
missing. Under runtime ownership, this expiration check moves into the
dispatcher; the canary asserts it is not skipped.

### J. Lazy expiration is observed inside EXEC

Setting `PEXPIREAT` outside the transaction and then reading the same
key inside EXEC must report the key as expired. The owner loop's
intra-EXEC dispatch must read the current DB state, not a snapshot
taken at `MULTI` time.

### K. Expiration applied INSIDE MULTI affects later queued commands

`PEXPIREAT` queued first, then `EXISTS` / `GET` / `TTL` later in the
same batch — the writes of the earlier queued command must be visible
to the later queued commands at execute time. Intra-batch effect
visibility is the most subtle property the owner loop has to preserve.

### L. Pub/sub introspection is side-effect-free

`PUBLISH` against a channel with no subscribers returns `:0` and must
not corrupt connection state. `PUBSUB CHANNELS` / `NUMSUB` / `NUMPAT`
must report empty registries on a fresh server. Bracketing them with
`SET` / `GET` confirms ordinary commands continue to work. Real pub/sub
delivery requires a second client and is NOT exercised here.

### M. Selected DB persists per-connection

`SELECT N` is per-client state. Writes to DB 1 must not be visible from
DB 0 and vice versa. Switching back must observe the original DB's
contents. Under the owner loop the per-client DB index moves from a
field on the per-thread `Client` into a `ClientSlot`; this canary
asserts that move did not silently share DB indices across slots.

### N. SELECT inside MULTI

`SELECT` is queued like any other write. The write that follows a
queued `SELECT 1` must land in DB 1, and the connection's DB after
EXEC must reflect the LAST `SELECT` that ran during EXEC. This is the
trickiest cross-DB case in the protocol and is exactly the kind of
intra-batch state the runtime owner must replay correctly.

### O. Non-hanging blocking pops

`BLPOP` / `BRPOP` / `BLMOVE` / `BLMPOP` with data already present must
return immediately without blocking. This is the only blocking shape
safe inside a single-connection corpus: any blocking call against an
empty list with timeout 0 would hang the runner. Cross-client wakeups
are deliberately out of scope.

### P. WAIT 0 0

`WAIT 0 0` must return `:0` immediately on a standalone server with no
replicas. Asserts the owner loop does not unconditionally park the
client waiting for replication progress when none is needed.

### Q. CLIENT name lifecycle including RESET

`CLIENT SETNAME` then `CLIENT GETNAME` round-trips the name; `RESET`
clears it and returns the connection to DB 0; a follow-up `SETNAME`
confirms the connection remains writable post-RESET. The existing
`08-server.txt` exercises `SETNAME`/`GETNAME` plus `RESET`+`PING`; this
canary widens the assertion to include the post-RESET getname and a
fresh setname round-trip.

### R. Cleanup

`FLUSHALL` and per-DB `DBSIZE` assert both touched DBs are empty at end
of script. Keeps the corpus self-contained so re-running it from a
fresh server is deterministic.

## What this corpus does NOT change

- It does not edit any existing corpus file. Differences in older
  corpora remain attributable to their original packets.
- It does not introduce any normalizer. The class is `byte_exact`. Any
  divergence is a real wire incompatibility under the runtime-owner
  invariants enumerated above.
- It does not change the Rust server's behavior. Failures here are
  bugs to chase; they are not failures of the canary.

## Expected initial state

When this canary first lands (`runtime-owner-1-canary-corpus`), the Rust
server is not yet runtime-owner shaped. A back-to-back run against the
pinned upstream binary and the current `target/debug/redis-server`
produces:

- 125 / 131 commands PASS byte-for-byte
- 6 commands FAIL, all inside the listed concern surface:
  - Section H: `EXEC` after nested `MULTI` — C returns `EXECABORT`,
    current Rust returns the partially queued batch result. The Rust
    `multi.rs` is not setting `DIRTY_EXEC` on nested MULTI errors.
  - Section N: `SELECT` inside MULTI is applied at queue time in the
    current Rust path instead of being queued and replayed at EXEC time,
    so the queued `SET` lands in the wrong DB and the queued `GET` reads
    the wrong DB. This is the canary the packet was specifically asked
    to write.
  - Section Q: `CLIENT GETNAME` after `RESET` returns an empty bulk
    string in current Rust where C returns nil. Wire-format divergence
    on the cleared-name path.

The runner packet `runtime-owner-post-canary-oracle` records this as the
new baseline. Subsequent fixer packets must close these divergences by
changing the *Rust dispatcher*, not by changing the corpus or adding a
normalizer. The corpus is the spec; the Rust server is what changes.

## Re-running

The smoke runner picks up `harness/oracle/corpus/*.txt` automatically, so:

```sh
bash harness/oracle/smoke.sh --skip-build
```

will include `21-runtime-owner-canaries` in its per-script roll-up. The
follow-up packet `runtime-owner-post-canary-oracle` runs this gate and
records evidence; per the binding decision doc, the runtime-owner scaffold
packet (`runtime-owner-2-scaffold-types`) does not dispatch until this
oracle is green.

## Known follow-ups (NOT this packet)

- A multi-connection runner is needed before WATCH-invalidation,
  pub/sub-delivery, and blocking-wakeup canaries can land. That runner
  is out of scope for `runtime-owner-1-canary-corpus`.
- A separate `eval` canary corpus would cover script-step replication-safe
  command ordering once the Rust server's EVAL error wording matches
  upstream byte-for-byte. Tracked under the broader scripting work, not
  here.
- An AOF/replication propagation oracle exists in
  `harness/oracle/rdb-corpus` and `harness/oracle/rdb-diff`; this canary
  intentionally does not duplicate that surface.
