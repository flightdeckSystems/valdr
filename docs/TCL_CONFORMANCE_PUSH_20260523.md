# TCL Conformance Push — 2026-05-23

This note captures the method adjustment before the next long run. The goal is
not to chase isolated green tests; it is to move official Valkey TCL coverage
while preserving the existing wire, RDB, and performance evidence.

## Current State

The post-DUMP frontier survey established a focused 10-file baseline:

```text
173 counted passes
0 counted failures
4 files without Test Summary
```

The HLL and SORT packets then changed the frontier:

- `unit/hyperloglog` now reaches a summary at **26 pass / 0 fail** after
  `PFSELFTEST` and live `hll-sparse-max-bytes` sparse-to-dense behavior were
  wired through the existing HLL representation and live config path. Evidence:
  `harness/oracle/results/tcl-survey/20260523T143708Z/unit__hyperloglog.json`.
- `unit/sort` now reaches a counted summary at **49 pass / 5 fail** after
  `COMMAND GETKEYS` for SORT/SORT_RO and the legacy `list-max-ziplist-size`
  CONFIG alias were wired through normal metadata paths. Evidence:
  `harness/oracle/results/tcl-survey/20260523T141126Z/unit__sort.json`.
  Remaining failures are list encoding, script SORT nosort behavior, and
  bad-double error text.
- `unit/slowlog` now reaches a counted summary at **13 pass / 0 fail** after
  the minimal `FUNCTION LOAD` / `FCALL` bridge was added on top of the existing
  Lua runtime and normal dispatcher. Evidence:
  `harness/oracle/results/tcl-survey/20260523T154121Z/unit__slowlog.json`.
- `unit/scripting` now advances through the function-mode setup and remains a
  true scripting frontier. The focused survey still aborts before Test Summary
  at missing `WAITAOF`, with earlier failures in multi-bulk Lua conversion,
  caller DB preservation around `SELECT`, and `WAIT` behavior. Evidence:
  `harness/oracle/results/tcl-survey/20260523T154121Z/unit__scripting.json`.

The earlier packet graph made a correct local decision but a bad run-level
decision: it serialized `slowlog` and `functions` behind the old `sort` packet.
When `sort` was marked blocked, the rest of the night could not advance even
though other frontiers were independent.

## Method Adjustment

Use branch-style frontier packets:

```text
baseline-v2
  ├─ sort connection metadata ── post-sort survey
  ├─ hll config/selftest      ── post-hll survey
  ├─ slowlog core edges      ── post-slowlog survey
  └─ functions minimal       ── post-functions survey
                                  ↓
                         expanded core survey
```

This gives the loop four useful branches after the baseline. If one branch
blocks, the other branches still remain legitimate work for a wrapper or
operator to resume. Every branch has concrete upstream anchors and a focused
TCL survey gate.

## Packet Standards For This Run

- Read the upstream C and TCL test first. The packet notes include exact
  `reference/valkey/src/...` and `reference/valkey/tests/...` anchors.
- Use the shared command dispatcher. Do not bypass `redis_commands::dispatch`
  with benchmark-only or test-only paths.
- Treat CONFIG aliases and COMMAND metadata as metadata, not command semantics.
- Do not fake subsystem completion. If a feature cannot execute loaded code,
  `FUNCTION LOAD` must fail rather than return a misleading OK.
- Keep runner evidence telemetry-scoped. The official TCL survey is a packet
  generator and regression signal; it is not by itself a public compatibility
  claim until the covered files and exclusions are documented in
  `docs/CONFORMANCE.md`.
- Keep old blocked packet IDs blocked. Use v2 packet IDs after a commit changes
  the frontier, so the ledger remains historically accurate.

## Why This Is Faster

The speedup is not from asking agents to type faster. It comes from removing
avoidable waiting:

- The prior graph had one critical path: HLL → SORT → SLOWLOG → FUNCTIONS.
- The new graph has one short baseline followed by independent branches.
- The focused runner after each branch tells the next architect packet what
  actually moved, instead of relying on transcript prose.
- A final expanded core survey creates the next packet-generation surface across
  single-node core files, not just the original 10-file unswept set.

## Current V2 Packets

- `tcl-core-frontier-baseline-v2`
- `tcl-sort-connection-metadata-v2`
- `tcl-hll-config-selftest-v2`
- `tcl-slowlog-core-edges-v2`
- `tcl-functions-load-fcall-minimal-v2`
- `tcl-core-expanded-survey-v1`

The completion profile now tracks this v2 graph as the active required
conformance frontier. The old serial `tcl-overnight-frontier-20260523-*`
criteria remain in the file as non-required historical evidence.
