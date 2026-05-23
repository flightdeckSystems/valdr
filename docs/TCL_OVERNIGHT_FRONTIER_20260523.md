# TCL Overnight Frontier — 2026-05-23

Status: HLL PFDEBUG/config/selftest, SORT wire/metadata, SLOWLOG core-edge, and
minimal FUNCTION LOAD/FCALL packets applied; remaining frontier updated.

## Goal

Use the current post-DUMP baseline to push the next official TCL frontier
without drifting away from a faithful single-node Valkey-compatible server.
The loop should prefer bounded conformance packets with objective survey gates
over local one-off fixes.

Current focused frontier baseline after `tcl-frontier-baseline-after-dump`:

```text
10 files surveyed
173 passed tests counted
0 failed tests counted
0 timed out
4 files aborted before Test Summary
```

Counted-green files: `unit/bitops`, `unit/bitfield`, `unit/geo`,
`unit/scan`, `unit/dump`.

Remaining no-summary files are true missing-subsystem frontiers:

- `unit/scripting` — function-mode setup now runs, but the file still aborts
  before Test Summary at missing `WAITAOF`, with earlier EVAL semantic failures
  around multi-bulk conversion, `SELECT`, and `WAIT`.

Focused frontier changes after packet runs:

- `unit/hyperloglog` — 26 pass / 0 fail.
- `unit/sort` — 49 pass / 5 fail after `COMMAND GETKEYS` and the
  `list-max-ziplist-size` CONFIG alias were wired without bypassing normal
  dispatch.
- `unit/slowlog` — 13 pass / 0 fail after the minimal function bridge lets the
  slowlog function-mode tail execute.

Post-`tcl-hll-pfdebug-dispatch` focused update:

```text
unit/hyperloglog
23 passed tests counted
3 failed tests counted
0 timed out
0 files aborted before Test Summary
```

Evidence:
`harness/oracle/results/tcl-survey/20260523T052712Z/unit__hyperloglog.json`.
The remaining HLL failures at that point were `PFSELFTEST` and config-driven
`hll-sparse-max-bytes` sparse-to-dense behavior. Treat the combined frontier as
196 counted passes, 3 counted failures, and 3 no-summary files only as a
historical telemetry projection from the prior 10-file baseline plus this
focused HLL run.

Post-`tcl-hll-config-selftest-v2` focused update:

```text
unit/hyperloglog
26 passed tests counted
0 failed tests counted
0 timed out
0 files aborted before Test Summary
```

Evidence:
`harness/oracle/results/tcl-survey/20260523T143708Z/unit__hyperloglog.json`.
This clears the `PFSELFTEST` abort and config-driven `hll-sparse-max-bytes`
sparse-to-dense promotion gap without rewriting PFCOUNT/PFMERGE or bypassing
normal dispatch.

Post-`tcl-sort-connection-metadata-v2` focused update:

```text
unit/sort
49 passed tests counted
5 failed tests counted
0 timed out
0 files aborted before Test Summary
```

Evidence:
`harness/oracle/results/tcl-survey/20260523T141126Z/unit__sort.json`.
This clears the `COMMAND GETKEYS` abort and legacy CONFIG alias gap. The
remaining failures are two list encoding assertions, script SORT nosort
behavior, and two bad-double error-text mismatches. Treat the combined frontier
as 248 counted passes, 5 counted failures, and 2 no-summary files only as a
telemetry projection from the prior 10-file baseline plus focused HLL and SORT
runs.

Post-`tcl-slowlog-core-edges-v2` focused update:

```text
unit/slowlog
0 counted tests due no Test Summary
0 packet-scoped failures before abort
0 timed out
1 file aborted before Test Summary
```

Evidence:
`harness/oracle/results/tcl-survey/20260523T150459Z/unit__slowlog.json`.
This clears strict `SLOWLOG GET` count validation, blocked-command delayed
logging, original blocked argv logging, and the generated `SKIP_COMMANDLOG`
metadata path for `EXEC`. The remaining stop is `FUNCTION LOAD`, which belongs
to the functions packet. Treat the combined frontier as unchanged for counted
passes until `unit/slowlog` reaches a real Test Summary.

Post-`tcl-functions-load-fcall-minimal-v2` focused update:

```text
unit/slowlog
13 passed tests counted
0 failed tests counted
0 timed out
0 files aborted before Test Summary

unit/scripting
0 counted tests due no Test Summary
3 reported failures before abort
0 timed out
1 file aborted before Test Summary
```

Evidence:
`harness/oracle/results/tcl-survey/20260523T154121Z/unit__slowlog.json` and
`harness/oracle/results/tcl-survey/20260523T154121Z/unit__scripting.json`.
This clears the function-load stop for the focused slowlog tail and lets
function-mode scripting run through the existing Lua bridge. The remaining
scripting frontiers are `WAITAOF` command coverage and EVAL semantics, outside
this packet scope.

## Packet Strategy

1. `tcl-frontier-baseline-after-dump`
   - Runner-only snapshot. This proves the run starts from the expected
     173/0/4 frontier.

2. `tcl-hll-pfdebug-dispatch`
   - Applied. `PFDEBUG` now uses the shared dispatcher and the existing HLL
     bytes for `GETREG`, `DECODE`, `ENCODING`, and `TODENSE`. The focused
     `unit/hyperloglog` survey reaches a counted summary at 23 pass / 3 fail.

3. `tcl-hll-config-selftest`
   - Applied in `tcl-hll-config-selftest-v2`. `PFSELFTEST` now exercises the
     existing HLL register and approximation paths, and
     `hll-sparse-max-bytes` drives PFADD sparse-to-dense promotion through
     `LiveConfig`. PFCOUNT and PFMERGE semantics remain on the stored HLL
     representation.

4. `tcl-sort-wire-minimal`
   - Applied. `sort.rs` is compiled into the crate, `SORT` and `SORT_RO` are
     registered in the shared dispatcher, and minimal context helpers support
     BY/GET/LIMIT/ASC/DESC/ALPHA/STORE/SORT_RO STORE rejection.
   - Follow-up `tcl-sort-connection-metadata-v2` applied. `COMMAND GETKEYS`
     now reports SORT/SORT_RO key positions from command metadata, and the
     legacy `list-max-ziplist-size` alias maps to the live listpack setting.

5. `tcl-slowlog-core-edges`
   - Applied in `tcl-slowlog-core-edges-v2`. Core SLOWLOG semantics that are
     independent of scripting functions now cover strict count argument
     validation, delayed blocked-command logging through the list wake path,
     original argv preservation for blocked commands, and generated
     `SKIP_COMMANDLOG` metadata for `EXEC`.

6. `tcl-functions-load-fcall-minimal`
   - Applied in `tcl-functions-load-fcall-minimal-v2`. Minimal Lua function
     bridge for `FUNCTION LOAD [REPLACE]` and `FCALL`/`FCALL_RO`, backed by the
     existing EVAL machinery and shared command dispatcher. Scope is
     deliberately smaller than full Valkey functions: no full metadata,
     persistence, engines API, async kill, listing, or cluster behavior.

Every implementation packet is followed by the same `tcl-survey-unswept`
runner. The loop is allowed to stop on repeated packet failure; repeated
failure should produce a blocker row instead of burning the night.

## Non-Goals

- No cluster/module/Sentinel/TLS expansion.
- No benchmark-only shortcuts.
- No weakening of wire-diff or RDB oracles.
- No broad workspace formatting churn.
- No fake function API that returns OK but cannot execute the loaded body.
