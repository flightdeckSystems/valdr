# harness/bench — performance-history dashboard

The dashboard at `http://localhost:8022/` (when served) is the visual face of the
valkey-rs port. It is a single static HTML page that plots throughput-vs-upstream
ratios across the project's commit history. Every line should head toward `1.00x`
(parity with upstream Valkey on the same workload and hardware).

## Files in this tree

| Path | Role |
|---|---|
| `history.py` | **Source of truth.** Reads evidence, writes `history/history.json` + `history/index.html`. |
| `history/index.html` | Generated artifact. **Do not hand-edit** — regenerating clobbers it. |
| `history/history.json` | Generated artifact. The chart's data payload. |
| `run.sh` | The default benchmark runner; emits TSVs into `bench/results/`. |
| `results/*.tsv` | Raw benchmark TSVs (the "raw points" series). |

## Critical: edit `history.py`, not `history/index.html`

`history/index.html` is built by `render_html(history)` inside `history.py`. Any
direct edit to the static file survives only until the next `python
harness/bench/history.py` run. If you change the chart drawing, the styling, or
the page layout, **make the edit in `history.py` and regenerate**:

```bash
python harness/bench/history.py            # rebuild once
python harness/bench/history.py --serve    # rebuild on demand + serve at :8022
```

The currently-running server may be a plain `python -m http.server --directory
harness/bench/history`, which does **not** auto-rebuild. Check `lsof -i :8022`
before assuming the file you see is fresh. Prefer `history.py --serve` because
it rebuilds when the ledger/results are newer than the cached HTML.

## Data pipeline

Two independent feeds combined per chart:

1. **Curated packet evidence** — `harness/evidence/ledger.jsonl` +
   `harness/evidence/runs/*` blobs. One point per completed benchmark packet.
   These are the "Curated …" series.
2. **Raw TSVs** — `harness/bench/results/*-{profile-matrix,hotspots,calltree,legacy}.tsv`.
   Many points per commit. These are the "Raw …" / "Legacy …" series and the
   long history at the top of the page.

Both are keyed by code commit (the source-side commit being benchmarked),
**not** by the runner-side commit that produced the TSV. That distinction is
why every chart shows multiple points stacked on the same x-position.

`signature` in `history.json` is the cache key — `(ledger_mtime_ns,
results_mtime_ns, point_count, raw_point_count)`. `needs_rebuild()` compares
against the embedded signature in the existing `index.html` and skips work if
nothing changed. If you've edited `history.py` itself, force a rebuild with
`python harness/bench/history.py` (no `--serve`).

## Chart conventions

- Y-axis is always a **ratio**: `valkey-rs throughput / upstream Valkey
  throughput` on the matching workload. `1.00x` is the goal line.
- Series colors are stable across charts — e.g. profile-matrix is always blue
  (`#2f6fed`), hotspots orange (`#c16a1a`), calltree green (`#0f8f68`). New
  series should be defined in the `SERIES_DEFS` / `RAW_SERIES_DEFS` /
  `LEGACY_*_SERIES_DEFS` blocks at the top of `history.py`.
- `drawChart(...)` takes an `opts` parameter. Set `parityLine: true` to draw a
  dashed reference line at y=1.0 with a "parity (1.00x)" label. Set
  `alignedXAxis: true` for horizontal commit-hash labels with tick marks
  instead of the default rotated style. Added 2026-05-22. Currently used by
  the Granular Raw TSV, Full Matrix — Core Commands, and Curated Packet
  Evidence charts. The remaining charts (Full Matrix — Data Structures, Full
  Matrix — Range Workloads, GET Signals) still use the rotated style —
  enable them per-chart as you confirm each one reads well.
- Per-chart benchmark spec blocks use the `.bench-info` style (a definition
  list with uppercase label column + content column). The Curated Packet
  Evidence and Full Matrix — Core Commands sections have one — see them for
  the pattern. When you add a new chart whose workload isn't obvious from the
  title, include one. The contents should match the runner script that
  actually produced the data (e.g. `run-profile-matrix.sh` for the curated
  chart, `run.sh` defaults for the Full Matrix charts) — don't paraphrase
  from memory.
- For section description paragraphs and `.bench-info` content: describe what
  the benchmark *does* in plain English (commands tested, workload shape, how
  to read the ratio). **Do not point users at internal script paths** like
  `harness/bench/run.sh` — those are an implementation detail, not an
  explanation. If you want to credit the source, do it once in CLAUDE.md, not
  in every section header.

## Adding a new chart section

1. Add a `<section class="panel"> … <svg id="…-chart"> …` block to the HTML
   template string in `render_html()`.
2. Define the series in a new `*_SERIES_DEFS` list near the existing ones.
3. Build the series in `build_history()` (look at `build_series` for the
   pattern — it groups raw points by `runner_kind`/`field`).
4. Call `drawChart("…-chart", "…-legend", […series ids…], HISTORY.foo_series,
   FOO_SERIES, HISTORY.raw_points, {parityLine: true, alignedXAxis: true})`
   in the JS block. Use the opts unless there's a reason not to — the parity
   reference is what makes the chart readable.
5. Regenerate and screenshot before declaring it done.

## Things to know before redesigning

- The page is **one file with no build step** by design — `python
  http.server` is enough to share it. Keep it that way. No webpack, no
  framework imports.
- All charts share `drawChart()`. Per-chart styling lives in the `opts`
  argument, not in CSS or per-chart drawing functions.
- The tooltips use a single shared `#chart-tooltip` div positioned by
  `clientX/clientY`. SVG `<title>` is the fallback for accessibility but the
  HTML tooltip is the primary UX.
- The page is currently ~10k px tall. The bottom three tables (curated
  points, raw points, annotations) are flat dumps with no pagination —
  expected to stay that way until row counts get painful.
