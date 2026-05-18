# TCL test suite — dashboard

Last updated: 2026-05-17
Commit at measurement: 35795ac
Total TCL surface measured: 660 PASS / 182 FAIL / ~300 BLOCKED (unreachable behind single aborts)

Note: PASS/FAIL counts reflect only what the harness recorded. BLOCKED is
an estimate of tests the harness never reached due to file-abort conditions.
The three source docs were all measured against build `59bbe91` (Round 9
head) except `unit/type/string` which reflects the Round 10a re-run.

## Per-file pass rates

| File | Pass | Fail | Tag-skipped | --skiptest | Notes |
|---|---|---|---|---|---|
| unit/type/string | 92 | 24 | 9 | 20 | Round 10a re-run; up from 81 in Round 9 |
| unit/type/list | 185 | 61 | ? | 3 | first-run baseline; -resp3 tag denied |
| unit/type/hash | 1 | 1 | 0 | 0 | aborts on bare assert_encoding at line 41; ~150 tests unreachable |
| unit/type/set | 61 | 52 | 5 | 1 | 48 of 52 fails = encoding aliases |
| unit/type/zset | 216 | 22 | 3 | 6 | ~50 more tests unreachable past line 2433 abort |
| unit/expire | 52 | 9 | 11 | 0 | race-flaky on restart_server; 33% of runs see only 3 passes |
| unit/incr | 14 | 0 | 1 | 0 | aborts on first INCRBYFLOAT; 14 trailing tests unreachable |
| unit/keyspace | 27 | 0 | 1 | 0 | aborts on COPY DB 10; 38 trailing tests unreachable |
| unit/quit | 3 | 0 | — | — | fully green |
| unit/type/list-2 | 2 | 0 | — | — | fully green |
| unit/limits | 0 | 1 | — | — | maxclients not enforced |
| unit/info-command | 1 | 4 | — | — | INFO commandstats missing per-command fields |
| unit/auth | 0 | 4 | — | — | AUTH not implemented; aborts after 4 setup errors |
| unit/protocol | 0 | 0+ | — | — | aborts on first test (empty query handling) |
| unit/sort | 0 | 0+ | — | — | aborts in fixture setup on assert_encoding listpack |
| unit/scan | 6 | 4 | — | — | 4 encoding-assertion fails then abort on unimplemented SCAN option |

Discrepancies: `unit/type/string` has two counts: 81 pass (Round 9, pre-fix)
and 92 pass (Round 10a, post-fix). The dashboard uses 92 as the current
figure. All other files were measured at the same `59bbe91` snapshot.

## Top failure categories (across all files)

1. **OBJECT ENCODING returns wrong alias for small collections**
   - Affects: hash.tcl (full file blocked after 1 fail), set.tcl (48 of 52 fails),
     zset.tcl (9 fails + second start_server block blocked), sort.tcl (full file
     blocked in fixture setup), scan.tcl (4 fails), expire.tcl (some), string.tcl
     (3 int-encoding fails already addressed in Round 10a)
   - Hash always reports `hashtable` instead of `listpack`; set always reports
     `hashtable` instead of `listpack`/`intset`; zset always reports `skiplist`
     instead of `listpack`
   - Estimated unlock: ~180 tests in hash/set/zset alone; ~43 more in sort.tcl;
     plus scan.tcl and others — largest single fix in the suite
   - Status: Round 11 is targeting this

2. **Default `databases` config is fewer than 16; tests assume 16**
   - Affects: keyspace.tcl (COPY DB 10 aborts; 38 trailing tests unreachable)
   - Estimated unlock: ~30 tests
   - Status: Round 11 is targeting this (bump default to 16)

3. **Real blocking semantics not implemented (BLPOP/BRPOP/BLMOVE/BLMPOP)**
   - Affects: list.tcl (~45 of 61 fails)
   - Our stubs reply immediately with null array; tests that use
     `wait_for_blocked_client` or assert a deferred read receives payload fail
   - Estimated unlock: ~45 tests
   - Status: stub-only; requires a BlockedClient registry + ready_keys queue

4. **INCRBYFLOAT / HINCRBYFLOAT not implemented**
   - Affects: incr.tcl (14 tests blocked by single abort)
   - Estimated unlock: 14 tests
   - Status: not yet ported

5. **RESP3 / HELLO 3 not implemented**
   - Affects: list.tcl (requires -resp3 tag deny), zset.tcl (6 skiptests),
     string.tcl, others
   - Any `r hello 3` call raises NOPROTO and can abort the file
   - Status: not implemented

6. **Error-message wording mismatches**
   - EXPIRE/PEXPIRE family: we emit `ERR invalid expire time in command`
     instead of `ERR invalid expire time in 'expire' command` (3 expire.tcl fails)
   - SINTERCARD LIMIT: wrong error string (1 set.tcl fail)
   - ZUNIONSTORE/ZINTERSTORE WEIGHTS NaN wording (part of 8 zset.tcl fails)
   - ZMPOP / ZINTERCARD LIMIT wording (2 zset.tcl fails)
   - Empty RESP frame: `ERR empty command` instead of silent discard
     (aborts protocol.tcl)
   - Estimated unlock: ~10 tests cheap; protocol.tcl (31 tests) via 3-LOC fix

7. **EVAL / scripting and AUTH not in scope**
   - AUTH is unimplemented (`ERR unknown command 'auth'`); blocks auth.tcl (4 tests)
   - EVAL / Lua scripting not planned; affects tests across files
   - CLIENT IMPORT-SOURCE (Valkey-only) aborts expire.tcl's trailing block

## Next-action queue (after Round 11)

In rank order:

1. Land collection encoding promotion (listpack/intset/skiplist) — biggest single
   unlock (~200+ tests across hash, set, zset, sort, scan)
2. Bump default `databases` to 16 — unlocks ~30 keyspace tests for ~1 LOC
3. Implement INCRBYFLOAT / HINCRBYFLOAT — unlocks 14 incr.tcl tests
4. Fix EXPIRE error-wording format (`in 'cmd' command`) — unlocks ~6-8 tests
5. Fix `EXPIRE ... GT/LT` semantics for keys without TTL — 3 expire.tcl tests
6. Fix empty-RESP-frame handling (drop silently) — unlocks protocol.tcl (31 tests)
7. Wire sort.rs into dispatch against current CommandContext API — unlocks sort.tcl
8. Land minimal blocking I/O (BlockedClient registry) — unlocks ~45 list.tcl tests
9. Implement ZRANGE BYLEX / BYSCORE unified argument parsing — ~10 zset tests

## `--skiptest` exclusions in active use

Tests skipped because they hang (not just fail) — without these the harness
blocks until the 20-minute timeout.

| File | Pattern | Reason |
|---|---|---|
| unit/type/string | `MSETEX keyspace notifications` | Requires wired pub/sub + PSUBSCRIBE deferring client; hangs |
| unit/type/string | `/SET with IFEQ` | Valkey-only conditional-set extension; our parser returns syntax error |
| unit/type/string | `/^LCS` | LCS handler returns unimplemented error; Tcl exception aborts file |
| unit/type/list | `BRPOPLPUSH does not affect WATCH while still blocked` | Undefined `$cmd` var in failure path raises and aborts file |
| unit/type/list | `/SORT` | sort_command uses obsolete CommandContext API; ERR unknown command aborts |
| unit/type/list | `/various encodings` | DUMP unimplemented; unset var trips next test |
| unit/type/set | `SRANDMEMBER count overflow` | i64::MIN triggers hang/OOM in SRANDMEMBER negative path |
| unit/type/zset | `/RESP3` | HELLO 3 raises NOPROTO and aborts file |
| unit/type/zset | `/BZPOP` | BZPOPMIN not registered; ERR unknown command aborts |
| unit/type/zset | `/BZMPOP` | Same as BZPOP |
| unit/type/zset | `/ZDIFF fuzzing` | Server crash or harness loss after this test; treated as hang-equivalent |
| unit/type/zset | `/ZRANGE BYLEX` | Returns ERR syntax error (unimplemented); uncaught abort |
| unit/type/zset | `/ZRANGE BYSCORE` | Same as BYLEX |
| unit/type/zset | `/ZRANGESTORE BYLEX` | Same as BYLEX |

No `--skiptest` exclusions added in Round 10b (datatypes) or Round 10c
(key-ops). The abort-based blockers in those rounds cannot be bypassed with
`--skiptest` because the failing assertions are outside `test {}` blocks.

## Source docs (preserved)

- docs/TCL_TRIAGE.md — Round 9 baseline + Round 10a update (string, list)
- docs/TCL_TRIAGE_DATATYPES.md — Round 10b baselines (hash, set, zset)
- docs/TCL_TRIAGE_KEYOPS.md — Round 10c baselines (expire, incr, keyspace, and smaller files)

These can stay as-is; this dashboard summarizes them.
