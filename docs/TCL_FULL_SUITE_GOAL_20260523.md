# TCL Full-Suite Goal - 2026-05-23

This note exists to prevent one recurring confusion: a focused TCL survey is
not the same thing as the upstream Valkey test-suite denominator.

## Definitions

**Full upstream TCL suite** means everything under `reference/valkey/tests/`.
On this checkout that is:

```text
245 .tcl files
4,299 test blocks
```

Counting command:

```bash
find reference/valkey/tests -type f -name '*.tcl' | wc -l
grep -rE '^\s*test\s+\{|^\s*test\s+"' reference/valkey/tests --include='*.tcl' | wc -l
```

**Current scoped conformance** means the subset we have historically reported
as product-quality evidence: wire-diff, RDB bidirectional, and a surveyed
single-node TCL slice.

**Focused frontier survey** means a packet-generation runner over a small list
of files. It is useful for finding the next work item, but it is not the total
upstream-suite count.

## Current Numbers

These numbers are different views, not interchangeable totals.

| View | Meaning | Current value |
|---|---|---:|
| Full upstream inventory | All Valkey TCL tests in this checkout | 4,299 test blocks |
| Historical scoped TCL claim | Cleanup-wave core unit-file survey | ~877 pass / ~73 fail |
| Latest focused frontier run | `tcl-survey-unswept`, 10 files, telemetry only | 266 counted pass / 0 counted fail / 1 no-summary file |
| Broader core runner inventory | `tcl-survey-core`, 15 selected single-node files | 1,160 source test blocks |

The useful mental model is therefore: we are around the first thousand counted
upstream TCL passes, but we are not yet reporting against the full 4,299-test
denominator.

## Goal

The Redis/Valkey port's conformance goal is to move the reporting denominator
to the full upstream TCL suite.

That does not mean pretending cluster, Sentinel, TLS, modules, or multi-node
replication are already supported. It means the dashboard should stop hiding
behind a scoped denominator. Unsupported areas should become explicit red,
skipped-by-policy, or product-decision rows until they are implemented or
intentionally waived.

## Execution Plan

1. Keep focused frontier runners for fast packet work.
2. Refresh `tcl-survey-core` after each frontier wave so the broader
   single-node count is current.
3. Add a generated suite inventory that records every TCL file, test-block
   count, tags denied, timeout/no-summary status, pass count, and fail count.
4. Expand from single-node unit files to all unit files.
5. Add the missing infrastructure runners for multi-node replication,
   Sentinel, TLS, cluster, and module-related tests.
6. Report the full 4,299-test denominator in the main conformance dashboard,
   with unsupported categories called out instead of omitted.

## Reporting Rules

Every TCL number must state:

- the file list or runner id;
- whether it is historical or from a fresh run;
- the tag deny policy;
- counted passes and failures;
- timeout/no-summary files;
- whether the number is a scoped claim, telemetry, or full-suite accounting.

If a document says only "TCL: N pass" without those qualifiers, treat it as
ambiguous and fix the document.
