#!/usr/bin/env python3
"""Terminal-friendly rendering of a profile-matrix TSV.

Default: a compact 4-column view (workload, ratio with a win marker, our rps,
upstream rps) that fits any terminal. `--wide` prints every column (latencies
too) for when you need the detail. Reads a path argument or stdin.
"""

import sys


def load(path: str | None):
    text = (open(path, encoding="utf-8") if path else sys.stdin).read()
    meta, rows, header = {}, [], None
    for line in text.splitlines():
        if line.startswith("#"):
            parts = line[1:].strip().split("\t")
            if len(parts) == 2:
                meta[parts[0]] = parts[1]
            continue
        cells = line.split("\t")
        if header is None:
            header = cells
        else:
            rows.append(dict(zip(header, cells)))
    return meta, rows


def main() -> int:
    wide = "--wide" in sys.argv
    args = [a for a in sys.argv[1:] if not a.startswith("-")]
    meta, rows = load(args[0] if args else None)
    if not rows:
        print("no matrix rows found")
        return 1

    print(f"commit {meta.get('commit', '?')} · {meta.get('cpu', '?')} · "
          f"ratio = valdr_rps / valkey_rps  (>1.00 = we win)")

    if wide:
        cols = list(rows[0].keys())
        widths = {c: max(len(c), max(len(r[c]) for r in rows)) for c in cols}
        print("  ".join(c.ljust(widths[c]) for c in cols))
        for r in rows:
            print("  ".join(r[c].ljust(widths[c]) for c in cols))
        return 0

    pw = max(len(r["profile"]) for r in rows)
    print(f"  {'workload':<{pw + 16}}  {'ratio':>8}   {'valdr rps':>12}  {'valkey rps':>12}")
    print(f"  {'-' * (pw + 16)}  {'-' * 8}   {'-' * 12}  {'-' * 12}")
    last_profile = None
    for r in rows:
        ratio = float(r["ratio"])
        mark = "✓" if ratio >= 1.0 else " "
        cmd = r["command"].split(" ")[0][:15]
        prof = r["profile"] if r["profile"] != last_profile else ""
        last_profile = r["profile"]
        workload = f"{prof:<{pw}} {cmd:<15}"
        print(f"  {workload}  {ratio:>6.2f}x {mark}  "
              f"{int(float(r['rust_rps'])):>12,}  {int(float(r['reference_rps'])):>12,}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
