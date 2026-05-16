#!/usr/bin/env python3
"""Generate harness/file-deps.tsv from reference/valkey/src/*.{c,h}.

Applies project-architectural pattern rules: each Valkey C/H file is
assigned to one crate + a target Rust path + a pilot-phase tag.

The rules are encoded as `(regex, crate, rust_path_template, phase)`
tuples. Match order matters — first hit wins. Files matching no rule
get crate=`UNASSIGNED`, which is a soft warning (the architect can
either add a rule or explicitly mark the file out-of-scope).

Productionization angle: the rules table IS the project-architectural
knowledge. The walk-and-classify mechanism is generic and could be
extracted to port-harness/lib/file_classifier.py later if a second C
target adopts the same pattern.

Usage:
    python3 harness/gen-file-deps.py            # write harness/file-deps.tsv
    python3 harness/gen-file-deps.py --check    # exit 1 if regen would change anything
    python3 harness/gen-file-deps.py --print    # to stdout
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SRC_DIR = ROOT / "reference" / "valkey" / "src"
OUTPUT = ROOT / "harness" / "file-deps.tsv"


# (filename_pattern, crate, rust_path_pattern, phase)
# Patterns are full-match against the basename. Use {stem} in rust_path
# to interpolate the stem (basename without .c/.h). Phase is one of:
#   pilot   — in the first pilot translation pass
#   later   — in scope, but a later phase
#   skip    — out of scope (cli, tests, benchmarks, build tooling)
#   defer   — needs architectural decision before assignment
RULES: list[tuple[str, str, str, str]] = [
    # Pilot scope: protocol + minimal server + a few commands.
    (r"resp_parser\.[ch]", "redis-protocol", "src/parser.rs", "pilot"),
    (r"networking\.[ch]",  "redis-core",     "src/networking.rs", "pilot"),
    (r"t_string\.[ch]",    "redis-commands", "src/string.rs", "pilot"),
    # Core server state (Phase 2-3)
    (r"server\.[ch]",      "redis-server",   "src/server.rs", "later"),
    (r"db\.[ch]",          "redis-core",     "src/db.rs", "later"),
    (r"object\.[ch]",      "redis-core",     "src/object.rs", "later"),
    (r"expire\.[ch]",      "redis-core",     "src/expire.rs", "later"),
    (r"connection\.[ch]",  "redis-core",     "src/connection.rs", "later"),
    (r"call_reply\.[ch]",  "redis-core",     "src/call_reply.rs", "later"),
    (r"blocked\.[ch]",     "redis-core",     "src/blocked.rs", "later"),
    (r"notify\.[ch]",      "redis-core",     "src/notify.rs", "later"),
    (r"tracking\.[ch]",    "redis-core",     "src/tracking.rs", "later"),
    (r"acl\.[ch]",         "redis-core",     "src/acl.rs", "later"),
    (r"config\.[ch]",      "redis-core",     "src/config.rs", "later"),
    (r"debug\.[ch]",       "redis-core",     "src/debug.rs", "later"),
    (r"defrag\.[ch]",      "redis-core",     "src/defrag.rs", "later"),
    (r"unix\.[ch]",        "redis-core",     "src/unix.rs", "later"),
    (r"socket\.[ch]",      "redis-core",     "src/socket.rs", "later"),
    (r"latency\.[ch]",     "redis-core",     "src/latency.rs", "later"),
    (r"commandlog\.[ch]",  "redis-core",     "src/commandlog.rs", "later"),
    (r"slowlog\.[ch]",     "redis-core",     "src/slowlog.rs", "later"),
    (r"childinfo\.[ch]",   "redis-core",     "src/childinfo.rs", "later"),
    # Commands (one .rs per t_*.c)
    (r"t_list\.[ch]",      "redis-commands", "src/list.rs", "later"),
    (r"t_hash\.[ch]",      "redis-commands", "src/hash.rs", "later"),
    (r"t_set\.[ch]",       "redis-commands", "src/set.rs", "later"),
    (r"t_zset\.[ch]",      "redis-commands", "src/zset.rs", "later"),
    (r"t_stream\.[ch]",    "redis-commands", "src/stream.rs", "later"),
    (r"bitops\.[ch]",      "redis-commands", "src/bitops.rs", "later"),
    (r"hyperloglog\.[ch]", "redis-commands", "src/hyperloglog.rs", "later"),
    (r"geo\.[ch]",         "redis-commands", "src/geo.rs", "later"),
    (r"geohash.*\.[ch]",   "redis-commands", "src/geohash_{stem}.rs", "later"),
    (r"pubsub\.[ch]",      "redis-commands", "src/pubsub.rs", "later"),
    (r"multi\.[ch]",       "redis-commands", "src/multi.rs", "later"),
    (r"sort\.[ch]",        "redis-commands", "src/sort.rs", "later"),
    # Foundation types
    (r"sds.*\.[ch]",       "redis-types",    "src/sds_{stem}.rs", "later"),
    # Data-structure encodings (deferred crate)
    (r"dict\.[ch]",            "redis-ds", "src/dict.rs", "defer"),
    (r"hashtable\.[ch]",       "redis-ds", "src/hashtable.rs", "defer"),
    (r"kvstore\.[ch]",         "redis-ds", "src/kvstore.rs", "defer"),
    (r"adlist\.[ch]",          "redis-ds", "src/adlist.rs", "defer"),
    (r"listpack.*\.[ch]",      "redis-ds", "src/listpack_{stem}.rs", "defer"),
    (r"quicklist\.[ch]",       "redis-ds", "src/quicklist.rs", "defer"),
    (r"ziplist\.[ch]",         "redis-ds", "src/ziplist.rs", "defer"),
    (r"intset\.[ch]",          "redis-ds", "src/intset.rs", "defer"),
    (r"rax\.[ch]",             "redis-ds", "src/rax.rs", "defer"),
    (r"zskiplist.*\.[ch]",     "redis-ds", "src/zskiplist.rs", "defer"),
    (r"endianconv\.[ch]",      "redis-ds", "src/endianconv.rs", "defer"),
    # Persistence (later crate)
    (r"rdb\.[ch]",         "redis-persist", "src/rdb.rs", "defer"),
    (r"aof\.[ch]",         "redis-persist", "src/aof.rs", "defer"),
    (r"rio\.[ch]",         "redis-persist", "src/rio.rs", "defer"),
    (r"functions\.[ch]",   "redis-scripting", "src/functions.rs", "defer"),
    (r"eval\.[ch]",        "redis-scripting", "src/eval.rs", "defer"),
    (r"script\.[ch]",      "redis-scripting", "src/script.rs", "defer"),
    (r"script_lua\.[ch]",  "redis-scripting", "src/lua_bridge.rs", "defer"),
    # Replication
    (r"replication\.[ch]", "redis-repl", "src/replication.rs", "defer"),
    (r"syncio\.[ch]",      "redis-repl", "src/syncio.rs", "defer"),
    # Cluster & Sentinel
    (r"cluster.*\.[ch]",   "redis-cluster",  "src/cluster_{stem}.rs", "defer"),
    (r"sentinel\.[ch]",    "redis-sentinel", "src/sentinel.rs", "defer"),
    (r"crc16.*\.[ch]",     "redis-cluster",  "src/crc16_{stem}.rs", "defer"),
    (r"crc64.*\.[ch]",     "redis-persist",  "src/crc64_{stem}.rs", "defer"),
    (r"crccombine\.[ch]",  "redis-persist",  "src/crccombine.rs", "defer"),
    (r"crcspeed\.[ch]",    "redis-persist",  "src/crcspeed.rs", "defer"),
    # Modules
    (r"module\.[ch]",      "redis-modules", "src/api.rs", "defer"),
    # Event loop (Phase 2-3 decision)
    (r"ae\.[ch]",          "redis-core",    "src/event_loop.rs", "defer"),
    (r"ae_(epoll|evport|kqueue|select)\.[ch]", "redis-core", "src/event_{stem}.rs", "defer"),
    (r"anet\.[ch]",        "redis-core",    "src/anet.rs", "defer"),
    (r"bio\.[ch]",         "redis-core",    "src/bio.rs", "defer"),
    # Memory / allocators
    (r"zmalloc\.[ch]",          "redis-core", "src/zmalloc.rs", "defer"),
    (r"mt19937.*\.[ch]",        "redis-core", "src/mt19937.rs", "defer"),
    (r"allocator_defrag\.[ch]", "redis-core", "src/allocator_defrag.rs", "defer"),
    # TLS — decide later
    (r"tls\.[ch]",         "redis-core", "src/tls.rs", "defer"),
    # Built-in CLIs and benchmarks — separate binaries, out of pilot
    (r"redis-cli\.[ch]",        "SKIP", "", "skip"),
    (r"valkey-cli\.[ch]",       "SKIP", "", "skip"),
    (r"redis-benchmark\.[ch]",  "SKIP", "", "skip"),
    (r"valkey-benchmark\.[ch]", "SKIP", "", "skip"),
    (r"redis-check-aof\.[ch]",  "SKIP", "", "skip"),
    (r"redis-check-rdb\.[ch]",  "SKIP", "", "skip"),
    (r"valkey-check.*\.[ch]",   "SKIP", "", "skip"),
    (r"cli_(commands|common)\.[ch]", "SKIP", "", "skip"),
    (r"asciilogo\.[ch]",        "SKIP", "", "skip"),
    (r"sha[12].*\.[ch]",        "SKIP", "", "skip"),
    (r"sparkline\.[ch]",        "SKIP", "", "skip"),
    (r"siphash\.[ch]",          "redis-core", "src/siphash.rs", "defer"),
    # Misc utility — most go to redis-core
    (r"util\.[ch]",        "redis-core", "src/util.rs", "later"),
    (r"setproctitle\.[ch]","redis-core", "src/setproctitle.rs", "defer"),
    (r"sha256\.[ch]",      "redis-core", "src/sha256.rs", "defer"),
    (r"solarisfixes\.[ch]","SKIP", "", "skip"),
    (r"localtime\.[ch]",   "redis-core", "src/localtime.rs", "defer"),
    (r"monotonic\.[ch]",   "redis-core", "src/monotonic.rs", "defer"),
    (r"version\.[ch]",     "redis-core", "src/version.rs", "later"),
    (r"release\.[ch]",     "redis-core", "src/release.rs", "later"),
    # Logreq + threaded I/O
    (r"logreqres\.[ch]",   "redis-core", "src/logreqres.rs", "defer"),
    (r"threads_mngr\.[ch]","redis-core", "src/threads_mngr.rs", "defer"),
    (r"timeout\.[ch]",     "redis-core", "src/timeout.rs", "later"),
    (r"lazyfree\.[ch]",    "redis-core", "src/lazyfree.rs", "later"),
    # Generated/build infrastructure — handle by chassis, not translation
    (r"commands_def\.[ch]","SKIP", "(generated — use harness/gen-command-registry.py)", "skip"),
    (r"commands\.[ch]",    "SKIP", "(generated; covered by command-registry)", "skip"),
    (r"fmacros\.[ch]",     "SKIP", "(C macro compat — not needed in Rust)", "skip"),
    (r"connhelpers\.[ch]", "redis-core", "src/conn_helpers.rs", "defer"),
    (r"debugmacro\.[ch]",  "SKIP", "(debug macros — Rust uses debug_assert)", "skip"),
    # Entry / shutdown
    (r"entry\.[ch]",       "redis-server", "src/entry.rs", "later"),
    # Eviction + LRU/LFU
    (r"evict\.[ch]",       "redis-core", "src/evict.rs", "defer"),
    (r"lrulfu\.[ch]",      "redis-core", "src/lrulfu.rs", "defer"),
    # FIFO / queues / locking
    (r"fifo\.[ch]",        "redis-core", "src/fifo.rs", "defer"),
    (r"queues\.[ch]",      "redis-core", "src/queues.rs", "defer"),
    (r"mutexqueue\.[ch]",  "redis-core", "src/mutexqueue.rs", "defer"),
    # Format helpers
    (r"fmtargs\.[ch]",     "SKIP", "(printf-format helpers — Rust has format!)", "skip"),
    (r"intrinsics\.[ch]",  "SKIP", "(C intrinsics — Rust std equivalents)", "skip"),
    # Fuzzer (test infra; out of pilot)
    (r"fuzzer_.*\.[ch]",   "SKIP", "(fuzzer test infra)", "skip"),
    # I/O threading
    (r"io_threads\.[ch]",  "redis-core", "src/io_threads.rs", "defer"),
    (r"memory_prefetch\.[ch]", "redis-core", "src/memory_prefetch.rs", "defer"),
    # LOLWUT (easter egg ASCII art commands)
    (r"lolwut.*\.[ch]",    "redis-commands", "src/lolwut_{stem}.rs", "defer"),
    # LZF compression
    (r"lzf.*\.[ch]",       "redis-persist", "src/lzf_{stem}.rs", "defer"),
    # Memtest (memory diagnostic, skip)
    (r"memtest\.[ch]",     "SKIP", "(memory diagnostic CLI)", "skip"),
    # Partial quicksort
    (r"pqsort\.[ch]",      "redis-ds", "src/pqsort.rs", "defer"),
    # Random / strtod / strl utilities
    (r"rand\.[ch]",        "redis-core", "src/rand.rs", "defer"),
    (r"valkey_strtod\.[ch]", "redis-core", "src/strtod.rs", "defer"),
    (r"strl\.[ch]",        "SKIP", "(strlcpy/strlcat compat — Rust has equivalents)", "skip"),
    # Rax allocator hook
    (r"rax_malloc\.[ch]",  "redis-ds", "src/rax_malloc.rs", "defer"),
    # RDMA transport — defer
    (r"rdma\.[ch]",        "redis-core", "src/rdma.rs", "defer"),
    # Module headers (separate from module.c)
    (r"redismodule\.[ch]", "redis-modules", "src/abi.rs", "defer"),
    (r"valkeymodule\.[ch]","redis-modules", "src/abi.rs", "defer"),
    # Scripting engine (newer abstraction over eval.c)
    (r"scripting_engine\.[ch]", "redis-scripting", "src/engine.rs", "defer"),
    # Assertions / cpu affinity / syscheck
    (r"serverassert\.[ch]","SKIP", "(C assertion macros — Rust has assert!/debug_assert!)", "skip"),
    (r"setcpuaffinity\.[ch]","redis-core", "src/cpu_affinity.rs", "defer"),
    (r"syscheck\.[ch]",    "redis-core", "src/syscheck.rs", "defer"),
    # Test helpers
    (r"testhelp\.[ch]",    "SKIP", "(C test infra; Rust uses cargo test)", "skip"),
    # Stream header (paired with t_stream.c)
    (r"stream\.[ch]",      "redis-commands", "src/stream.rs", "later"),
    # Valkey benchmark dataset
    (r"valkey-benchmark-dataset\.[ch]", "SKIP", "(benchmark dataset — separate tool)", "skip"),
    # Vector / vset (vector search — relatively new commands)
    (r"vector\.[ch]",      "redis-commands", "src/vector.rs", "defer"),
    (r"vset\.[ch]",        "redis-commands", "src/vset.rs", "defer"),
    # Zipmap (legacy)
    (r"zipmap\.[ch]",      "redis-ds", "src/zipmap.rs", "defer"),
]


def classify(filename: str) -> tuple[str, str, str]:
    stem = filename.rsplit(".", 1)[0]
    for pat, crate, rust_path_tpl, phase in RULES:
        if re.fullmatch(pat, filename):
            return (crate, rust_path_tpl.format(stem=stem), phase)
    return ("UNASSIGNED", "", "defer")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true",
                    help="Exit 1 if regen would change anything.")
    ap.add_argument("--print", action="store_true",
                    help="Print to stdout instead of writing the file.")
    args = ap.parse_args()

    if not SRC_DIR.is_dir():
        print(f"FATAL: {SRC_DIR} not found. Run harness/bootstrap-upstream.sh first.",
              file=sys.stderr)
        return 2

    files = sorted(p.name for p in SRC_DIR.iterdir() if p.suffix in (".c", ".h"))

    out_lines = [
        "# Auto-generated by harness/gen-file-deps.py from reference/valkey/src/*.{c,h}.",
        "# DO NOT hand-edit. To re-assign a file, change RULES in the generator and re-run.",
        "#",
        f"# Total files: {len(files)}",
        "# Columns (tab-separated):",
        "#   cfile   — basename in reference/valkey/src/",
        "#   crate   — target Rust crate (or SKIP)",
        "#   rust    — relative path under crates/<crate>/",
        "#   phase   — pilot | later | defer | skip",
        "",
    ]

    phase_counts: dict[str, int] = {}
    unassigned: list[str] = []
    for f in files:
        crate, rust, phase = classify(f)
        if crate == "UNASSIGNED":
            unassigned.append(f)
        out_lines.append(f"{f}\t{crate}\t{rust}\t{phase}")
        phase_counts[phase] = phase_counts.get(phase, 0) + 1

    content = "\n".join(out_lines) + "\n"

    if args.print:
        sys.stdout.write(content)
        return 0

    if args.check:
        old = OUTPUT.read_text() if OUTPUT.exists() else ""
        if old != content:
            print("STALE: harness/file-deps.tsv", file=sys.stderr)
            return 1
        print(f"OK: file-deps.tsv in sync ({len(files)} files)")
        return 0

    OUTPUT.write_text(content)
    print(f"wrote {OUTPUT.relative_to(ROOT)} ({len(files)} files)")
    for phase in ["pilot", "later", "defer", "skip"]:
        print(f"  {phase:<8} {phase_counts.get(phase, 0)}")
    if unassigned:
        print(f"  UNASSIGNED  {len(unassigned)}: {', '.join(unassigned[:5])}{'...' if len(unassigned) > 5 else ''}")
        print("  (add a rule to RULES in this script and re-run)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
