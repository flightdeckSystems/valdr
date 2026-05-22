#!/usr/bin/env python3
"""Backfill benchmark artifacts by rebuilding historical commits in worktrees.

This script exists to prevent a specific class of bad performance evidence:
running a benchmark at commit N while accidentally reusing a release binary
built at commit N-k. Each requested commit gets its own detached git worktree
and its own target directory, so the benchmarked binary is rebuilt from the
checked-out source for that commit.

The script copies generated benchmark artifacts back into the current checkout
under harness/bench/results and harness/bench/profiles. It does not append
ledger rows; those raw TSV/JSON artifacts are picked up by history.py's raw
series.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
DEFAULT_STATE_ROOT = ROOT / "target/bench-backfill"
RESULTS_REL = Path("harness/bench/results")
PROFILES_REL = Path("harness/bench/profiles")
REFERENCE_REL = Path("reference/valkey")


class BenchKind(Enum):
    LEGACY = "legacy"
    MATRIX = "matrix"
    HOTSPOTS = "hotspots"
    CALLTREE = "calltree"


@dataclass(frozen=True)
class BackfillResult:
    commit: str
    kind: BenchKind
    copied: list[Path]
    skipped: bool = False


def run(
    cmd: list[str],
    *,
    cwd: Path = ROOT,
    env: dict[str, str] | None = None,
    capture: bool = False,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd,
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
        check=True,
    )


def git_text(args: list[str], *, cwd: Path = ROOT) -> str:
    completed = run(["git", *args], cwd=cwd, capture=True)
    return completed.stdout.strip()


def resolve_commits(args: argparse.Namespace) -> list[str]:
    commits: list[str] = []

    for rev in args.rev_list:
        out = git_text(["rev-list", "--reverse", rev])
        commits.extend(line.strip() for line in out.splitlines() if line.strip())

    for path in args.commits_from_file:
        for line in path.read_text(encoding="utf-8").splitlines():
            line = line.split("#", 1)[0].strip()
            if line:
                commits.append(line)

    commits.extend(args.commits)

    seen: set[str] = set()
    resolved: list[str] = []
    for commit in commits:
        full = git_text(["rev-parse", "--verify", f"{commit}^{{commit}}"])
        if full not in seen:
            seen.add(full)
            resolved.append(full)
    return resolved


def short_commit(commit: str) -> str:
    return git_text(["rev-parse", "--short", commit])


def utc_stamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def artifact_files(worktree: Path) -> set[Path]:
    files: set[Path] = set()
    for rel in (RESULTS_REL, PROFILES_REL):
        root = worktree / rel
        if not root.exists():
            continue
        for path in root.rglob("*"):
            if path.is_file():
                files.add(path.relative_to(worktree))
    return files


def copy_artifacts(worktree: Path, rel_paths: set[Path]) -> list[Path]:
    copied: list[Path] = []
    for rel in sorted(rel_paths):
        src = worktree / rel
        dst = ROOT / rel
        dst.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, dst)
        copied.append(rel)
    return copied


def ensure_reference_available(worktree: Path) -> None:
    local_reference = ROOT / REFERENCE_REL
    if not local_reference.exists():
        run(["bash", "scripts/setup-reference.sh"], cwd=ROOT)

    reference_parent = worktree / REFERENCE_REL.parent
    reference_parent.mkdir(parents=True, exist_ok=True)
    reference_link = worktree / REFERENCE_REL
    if reference_link.exists() or reference_link.is_symlink():
        return
    try:
        reference_link.symlink_to(local_reference, target_is_directory=True)
    except OSError:
        # Fall back to the benchmark script's own setup-reference path.
        pass


def add_worktree(commit: str, worktree: Path) -> None:
    if worktree.exists():
        raise RuntimeError(f"refusing to reuse existing worktree: {worktree}")
    worktree.parent.mkdir(parents=True, exist_ok=True)
    run(["git", "worktree", "add", "--detach", str(worktree), commit], cwd=ROOT)
    ensure_reference_available(worktree)


def remove_worktree(worktree: Path) -> None:
    if not worktree.exists():
        return
    run(["git", "worktree", "remove", "--force", str(worktree)], cwd=ROOT)


def kind_command(kind: BenchKind, args: argparse.Namespace) -> list[str]:
    if kind is BenchKind.LEGACY:
        return [
            "bash",
            "harness/bench/run.sh",
            "--requests",
            str(args.legacy_requests),
            "--clients",
            str(args.legacy_clients),
            "--pipeline",
            str(args.legacy_pipeline),
            "--payload",
            str(args.legacy_payload),
            "--tests",
            args.legacy_tests,
        ]
    if kind is BenchKind.MATRIX:
        return ["bash", "harness/bench/run-profile-matrix.sh"]
    if kind is BenchKind.HOTSPOTS:
        return [
            sys.executable,
            "harness/bench/profile-hotspots.py",
            "--suite",
            args.suite,
        ]
    if kind is BenchKind.CALLTREE:
        return [
            sys.executable,
            "harness/bench/profile-calltree.py",
            "--suite",
            args.suite,
            "--profile-seconds",
            str(args.profile_seconds),
        ]
    raise AssertionError(kind)


def kind_script_rel(kind: BenchKind) -> Path:
    if kind is BenchKind.LEGACY:
        return Path("harness/bench/run.sh")
    if kind is BenchKind.MATRIX:
        return Path("harness/bench/run-profile-matrix.sh")
    if kind is BenchKind.HOTSPOTS:
        return Path("harness/bench/profile-hotspots.py")
    if kind is BenchKind.CALLTREE:
        return Path("harness/bench/profile-calltree.py")
    raise AssertionError(kind)


def result_globs(kind: BenchKind, commit: str) -> list[str]:
    if kind is BenchKind.LEGACY:
        return [f"*-{commit}.tsv"]
    if kind is BenchKind.MATRIX:
        return [f"*-{commit}-profile-matrix.tsv"]
    if kind is BenchKind.HOTSPOTS:
        return [f"*-{commit}-hotspots.tsv", f"*-{commit}-hotspots.json"]
    if kind is BenchKind.CALLTREE:
        return [f"*-{commit}-calltree.tsv", f"*-{commit}-calltree.json"]
    raise AssertionError(kind)


def has_existing_result(kind: BenchKind, commit: str) -> bool:
    results = ROOT / RESULTS_REL
    return any(any(results.glob(pattern)) for pattern in result_globs(kind, commit))


def benchmark_env(args: argparse.Namespace) -> dict[str, str]:
    env = os.environ.copy()
    for item in args.env:
        if "=" not in item:
            raise ValueError(f"--env expects KEY=VALUE, got {item!r}")
        key, value = item.split("=", 1)
        env[key] = value
    return env


def write_manifest(args: argparse.Namespace, results: list[BackfillResult]) -> Path:
    manifest = {
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "runner_commit": short_commit("HEAD"),
        "runner_from_current": bool(args.runner_from_current),
        "kinds": [kind.value for kind in [BenchKind(value) for value in (args.kind or [BenchKind.MATRIX.value])]],
        "results": [
            {
                "target_commit": result.commit,
                "kind": result.kind.value,
                "skipped": result.skipped,
                "copied": [str(path) for path in result.copied],
            }
            for result in results
        ],
    }
    path = ROOT / RESULTS_REL / f"{utc_stamp()}-backfill-manifest.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return path


def run_kind(
    *,
    worktree: Path,
    commit_short: str,
    kind: BenchKind,
    args: argparse.Namespace,
    env: dict[str, str],
) -> BackfillResult:
    if args.skip_existing and has_existing_result(kind, commit_short):
        print(f"==> {commit_short} {kind.value}: skip existing")
        return BackfillResult(commit_short, kind, [], skipped=True)

    script_rel = kind_script_rel(kind)
    script = worktree / script_rel
    if args.runner_from_current:
        current_script = ROOT / script_rel
        if not current_script.exists():
            raise RuntimeError(f"current checkout is missing runner script: {script_rel}")
        script.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(current_script, script)

    if not script.exists():
        print(f"==> {commit_short} {kind.value}: skip missing {script_rel}")
        return BackfillResult(commit_short, kind, [], skipped=True)

    if kind is BenchKind.LEGACY:
        # Older versions of harness/bench/run.sh required the release binary to
        # exist before the script started. Build explicitly so backfill can
        # replay those commits without depending on the script's current shape.
        run(["cargo", "build", "--release", "-p", "redis-server"], cwd=worktree, env=env)

    before = artifact_files(worktree)
    print(f"==> {commit_short} {kind.value}")
    run(kind_command(kind, args), cwd=worktree, env=env)
    after = artifact_files(worktree)
    copied = copy_artifacts(worktree, after - before)
    print(f"    copied {len(copied)} artifact(s)")
    return BackfillResult(commit_short, kind, copied)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Backfill benchmark artifacts from historical commits."
    )
    parser.add_argument("commits", nargs="*", help="Commit-ish values to benchmark.")
    parser.add_argument(
        "--rev-list",
        action="append",
        default=[],
        help="Add commits from `git rev-list --reverse REV`.",
    )
    parser.add_argument(
        "--commits-from-file",
        action="append",
        type=Path,
        default=[],
        help="Read commit-ish values from a file, one per line.",
    )
    parser.add_argument(
        "--kind",
        action="append",
        choices=[item.value for item in BenchKind],
        default=[],
        help="Benchmark kind to run. Defaults to matrix.",
    )
    parser.add_argument(
        "--legacy-requests",
        type=int,
        default=300_000,
        help="Request count for --kind legacy (harness/bench/run.sh).",
    )
    parser.add_argument(
        "--legacy-clients",
        type=int,
        default=50,
        help="Client count for --kind legacy (harness/bench/run.sh).",
    )
    parser.add_argument(
        "--legacy-pipeline",
        type=int,
        default=100,
        help="Pipeline depth for --kind legacy (harness/bench/run.sh).",
    )
    parser.add_argument(
        "--legacy-payload",
        type=int,
        default=64,
        help="Payload bytes for --kind legacy (harness/bench/run.sh).",
    )
    parser.add_argument(
        "--legacy-tests",
        default="set,get,incr,lpush,rpush,lpop,rpop,sadd,hset,spop,zadd,lrange_100,lrange_300,mset,ping_mbulk",
        help="Comma-separated test list for --kind legacy.",
    )
    parser.add_argument(
        "--suite",
        choices=["smoke", "big"],
        default="smoke",
        help="Suite for profile runners. Matrix runner ignores this.",
    )
    parser.add_argument(
        "--profile-seconds",
        type=int,
        default=8,
        help="Sampling seconds for calltree profiles.",
    )
    parser.add_argument(
        "--state-root",
        type=Path,
        default=DEFAULT_STATE_ROOT,
        help="Directory for temporary worktrees.",
    )
    parser.add_argument(
        "--keep-worktrees",
        action="store_true",
        help="Leave successful worktrees on disk for inspection.",
    )
    parser.add_argument(
        "--skip-existing",
        action="store_true",
        help="Skip a kind when matching result artifacts already exist.",
    )
    parser.add_argument(
        "--runner-from-current",
        action="store_true",
        help=(
            "Copy the selected benchmark runner script from the current checkout "
            "into each historical worktree before running it. The target binary "
            "is still rebuilt from the historical commit; only the measurement "
            "tooling is backfilled from the current checkout."
        ),
    )
    parser.add_argument(
        "--env",
        action="append",
        default=[],
        help="Extra environment for benchmark scripts, as KEY=VALUE.",
    )
    args = parser.parse_args()
    if not args.commits and not args.rev_list and not args.commits_from_file:
        parser.error("provide at least one commit, --rev-list, or --commits-from-file")
    return args


def main() -> int:
    args = parse_args()
    commits = resolve_commits(args)
    kinds = [BenchKind(value) for value in (args.kind or [BenchKind.MATRIX.value])]
    env = benchmark_env(args)

    all_results: list[BackfillResult] = []
    for commit in commits:
        commit_short = short_commit(commit)
        worktree = args.state_root / "worktrees" / commit_short
        add_worktree(commit, worktree)
        keep = args.keep_worktrees
        try:
            for kind in kinds:
                all_results.append(
                    run_kind(
                        worktree=worktree,
                        commit_short=commit_short,
                        kind=kind,
                        args=args,
                        env=env,
                    )
                )
        except Exception:
            keep = True
            print(f"left failed worktree for inspection: {worktree}", file=sys.stderr)
            raise
        finally:
            if not keep:
                remove_worktree(worktree)

    copied_total = sum(len(result.copied) for result in all_results)
    skipped_total = sum(1 for result in all_results if result.skipped)
    manifest = write_manifest(args, all_results)
    print(f"backfill complete: {len(all_results)} run(s), {copied_total} artifact(s), {skipped_total} skipped")
    print(f"manifest: {manifest.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
