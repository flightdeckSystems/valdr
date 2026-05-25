# Persistence + Restart Integration Wave — Recon Map (2026-05-25)

Agent 2 (Claude Opus 4.7), worktree `redis-rs-port-persist`, branch
`claude/persistence-wave-20260525` off main `6d89369`.

## Mission

Convert the persistence/integration bucket from hidden/abort → **counted**
files with named remaining failures. Move single-node counted coverage toward
60%. Make restart/load/rewrite/corruption behavior *visibly tested*, not just
RDB object-level.

## CRITICAL runner-tag insight

Run these files with **`--tags "-needs:repl"` ONLY**. Do **NOT** pass
`-external:skip`: every integration/persistence file is wrapped in
`tags {"... external:skip"}` because it spawns/kills/restarts servers — which
is exactly what we do when the runner launches our own binary. Passing
`-external:skip` makes the whole file report `0 passed, 0 failed`
(`[ignore]: Tag: external:skip denied`). This was the #1 reason an earlier
survey looked empty.

Evidence command (isolated copy, high baseport, never the shared default port):
```
cp -R reference/valkey/tests /tmp/persist/tests   # once
cd /tmp/persist
env VALKEY_BIN_DIR=<worktree>/target/debug \
  tclsh tests/test_helper.tcl --single integration/<file> \
  --clients 1 --skip-leaks --baseport 55000 --portcount 400 --tags "-needs:repl"
```

## Bucket state (with correct tags), 2026-05-25

| File | counted ok/err | runs to `The End`? | blocker that caps it |
|---|---|---|---|
| integration/corrupt-dump | 6 / 27 (was 0/8) | NO — aborts | long tail of fuzzer payloads; each `sanitize=no` test does a bare `r restore` expecting the load to *succeed*, we reject with `Bad data format` → uncaught throw. **Keystone `DEBUG SET-SKIP-CHECKSUM-VALIDATION` landed (`cad5b61`)** → 8→33. |
| integration/rdb | 6 / 9 | NO — aborts | **server crash** (`child process exited abnormally`) around "Test FLUSHALL aborts bgsave" (`rdb_changes_since_last_save` returns 0, expected >999) + several "Can't start server" config-rejection cases. PERSISTENCE-CORE LANE (mine). |
| integration/aof | 5 / 6 | NO — aborts | needs the **`valkey-check-aof` utility binary** (`exec valkey-check-aof` → "no such file"). Also truncated-AOF load semantics + "Server should have logged an error". |
| integration/aof-multi-part | 8 / 16 | NO — aborts | **AOF manifest parser bug**: `BGREWRITEAOF failed: Invalid AOF manifest file format at line 1` — our manifest serializer/parser mangles the `file ... seq ... type ...` format. PERSISTENCE-CORE LANE (mine). |
| integration/valkey-check-rdb | 0 / 8 | YES (counted) | needs the **`valkey-check-rdb` utility binary**. Already counted, all red. |
| unit/aofrw | 18 / 1 | NO — aborts | `ERR Function not found` from **FCALL** (`eval.rs:2436`) — SCRIPTING LANE (Codex active, contended; not mine). The visible test gap is also "Killing AOF child" not logged when `appendonly no` cancels an in-flight rewrite. |
| unit/dump | 13 / — | (CEDED to Codex Agent-1 core-visibility scout) | — |
| unit/other | 22-25 / 2 | partial | DEBUG reload/load cases; some counted. Lower priority. |

## Keystone primitives — status

- `DEBUG RELOAD` / `DEBUG LOADAOF`: **exist** (`connection.rs:1378/1435`).
- `DEBUG SET-SKIP-CHECKSUM-VALIDATION`: **landed this wave** (`cad5b61`),
  wired into `verify_dump_payload` + the RDB-file CRC path via a process-global
  flag (`crates/redis-core/src/rdb/load.rs::set_skip_checksum_validation`).
- `valkey-check-aof` / `valkey-check-rdb`: **missing binaries** — building
  minimal versions would unlock integration/aof's utility tests + the whole
  valkey-check-rdb file.

## DEEPER FINDINGS (after first implementation pass, 2026-05-25)

Landed this pass (branch `claude/persistence-wave-20260525`):
- `cad5b61` DEBUG SET-SKIP-CHECKSUM-VALIDATION → corrupt-dump 8→33.
- AOF manifest quoted-filename encode/parse + Unix-correct `path_is_base_name`.
- config-file `unquote_config_value` (sdssplitargs-style) → aof-multi-part
  "appendfilename contains whitespaces" PASSES; file 8→11/19.

**KEYSTONE BLOCKER — real fork-based background persistence.** The biggest
remaining cluster of aborts is not parser bugs; it's that BGSAVE/BGREWRITEAOF
are not separate forked child processes. The tests do
`set pid [get_child_pid 0]; exec kill -9 $pid` (bgsave cancel, "AOF multiple
rewrite failures", "failed bgsave prevents writes", etc.). With no real child
PID, the test kills the *server itself* → "child process exited abnormally"
aborts the file. Files blocked on this: **integration/rdb** (bgsave
cancel/schedule/abort) and the **aof-multi-part tail** (AOFRW-failure
injection). This is a MAJOR subsystem (fork, child writes RDB/AOF, parent
tracks `rdb_child_pid`/`aof_child_pid`, SIGCHLD reaping, cancel via signal) and
a product-scope decision — flag to the user, don't attempt blindly overnight.

**Strategic nuance:** the tests `kill -9` the save child by PID, so a thread
won't do — it needs a real child *process*. But `fork()` is inherently `unsafe`
in Rust and the project targets a zero-`unsafe` budget. So the options are
(a) accept `unsafe` fork in one contained module, or (b) a subprocess-based
save architecture that differs from upstream (the child can't COW-share the
dataset, so the parent must hand it the data). This is a genuine
constraint-vs-conformance decision for the user, not a mechanical fix.

**corrupt-dump: 8 → 44 counted** after adding the compact + legacy encoding
decoders (see "Encoding decoders landed" below). The remaining cap is the
`sanitize-dump-payload no` tests: they do a bare `r restore` of a corrupt
payload expecting the load to SUCCEED (structurally parseable but
semantically corrupt), then catch a runtime assertion/crash on access
(`use-exit-on-panic yes`). Safe Rust rejects corrupt data at the load
boundary, so the bare restore throws → file aborts. Replicating this needs a
"load leniently then assert on access" model that conflicts with safe Rust's
strict parsing — treat as out of scope / named failures.

**valkey-check-rdb: 0 → 5 counted.** Built the tool via argv[0] dispatch
(symlink `valkey-check-rdb` → redis-server) + `check_rdb_file` (offset
tracking, version classify, exact report lines incl. "Unknown object type N
in RDB file with {foreign,future} version M"). All 5 load/version tests pass,
including the Redis-2.6 v4 `encodings.rdb` after fixing a real correctness bug:
check_rdb_file was stripping a fixed 8-byte CRC footer, but rdb_ver < 5 has no
CRC — fixed to scan to the EOF opcode instead. Remaining 3: `--stats --format
info` histogram output (per-db per-type key counts; one variant also needs
FUNCTION2 opcode parsing) — a distinct output mode, not built.

**Encoding decoders landed (foundational, generalize beyond corrupt-dump):**
- `86c2f05` HASH/SET/ZSET listpack + SET intset load (Redis 7+ defaults), with
  duplicate/ordering rejection so corrupt payloads still fail.
- `e894042` HASH/ZSET/LIST ziplist + LIST_QUICKLIST v1 load (pre-7.0 dumps),
  via a new `rdb/ziplist.rs` decoder.
- RDB_TYPE_ZSET v1 (text-double scores) + RDB_TYPE_HASH_ZIPMAP (Redis 2.6).
  These unblock RESTORE/RDB-load of essentially every listpack/ziplist/intset/
  zipmap/text-zset-encoded value, which also helps the type/dump tests in
  other lanes.

**Remaining bounded targets (next):** check-rdb `--stats --format info` output
mode (3 tests); the v4 `encodings.rdb` int-encoded-object EOF bug (1 test);
and `valkey-check-aof` (clears integration/aof's missing-binary abort — needs
AOF manifest + base + incr parsing).

**valkey-check-rdb / valkey-check-aof**: confirmed these are the same binary
dispatched by `argv[0]` (symlink valkey-check-rdb/-aof → redis-server, branch
in `main()` on the basename). Spec for check-rdb (from `valkey-check-rdb.c`):
every line is `[offset <processed_bytes>] <msg>`; version classify with
`RDB_FOREIGN_VERSION_MIN=12`/`MAX=79` (foreign 12-79, future >79, ours=80);
messages `Foreign RDB version %d detected`, `Future RDB version %d detected`,
`--- RDB ERROR DETECTED ---` + `Unknown object type %d in RDB file with
{foreign,future} version %d`, `\o/ RDB looks OK! \o/`, and the relaxed variant
`\o/ RDB looks OK, but loading requires config 'rdb-version-check relaxed'`.
Needs the loader to expose byte-offset + an unknown-type error carrying the
offset. Bounded feature; clears integration/aof's abort (missing check-aof
binary) and greens valkey-check-rdb (8, already counted).

## Ranked next targets (by ROI, lane-safe for Agent 2)

1. **integration/aof-multi-part manifest parser** (mine): fix the manifest
   serialize/parse round-trip so `BGREWRITEAOF` stops aborting. ~16+ counted,
   self-contained in the AOF manifest code.
2. **integration/rdb crash** (mine): root-cause the `child process exited
   abnormally` near FLUSHALL-aborts-bgsave; also make `rdb_changes_since_last_save`
   track dirty count. Flips the file to counted.
3. **`valkey-check-rdb` + `valkey-check-aof` minimal binaries** (mine): a small
   bin crate each that loads the file through the existing rdb/aof loaders and
   prints the upstream-expected "OK"/"not valid"/"ok_up_to_line=N" lines.
   Unlocks integration/valkey-check-rdb (8) + integration/aof's utility tests.
4. **corrupt-dump long tail** (mine, hard): make `sanitize-dump-payload no`
   loads not hard-reject — the bare `r restore` cases expect success (then the
   server may crash on access, which upstream catches via `use-exit-on-panic`).
   This is the genuinely hard part (safe Rust can't "load corrupt then crash"
   the way C does); leave as named failures unless a clean approach emerges.

## Lane boundaries (do not clobber)

- AVOID `eval.rs`/`dispatch.rs` (scripting, Codex active) — so `unit/aofrw`'s
  FCALL abort is NOT ours; coordinate.
- AVOID `acl.rs` (ACL, Codex active).
- `unit/dump`, `unit/sort` ceded to Codex Agent-1 (core-visibility scout).
- Merge to main is deliberate: main's working tree has had other agents' dirty
  edits to `stream.rs`/`info.rs`; check `git status` before any file-checkout
  land and never overwrite uncommitted work.
