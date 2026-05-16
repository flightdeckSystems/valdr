#!/usr/bin/env bash
# Bootstrap the upstream source pinned in harness/source.toml.
# Idempotent: skips if already present at the right commit.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO="https://github.com/valkey-io/valkey"
COMMIT="0321a69e62f5148096d03358e26a1b884af3b969"
DEST="$ROOT/reference/valkey"

if [ -d "$DEST/.git" ]; then
    current=$(git -C "$DEST" rev-parse HEAD 2>/dev/null || echo none)
    if [ "$current" = "$COMMIT" ]; then
        echo "valkey already at pinned commit $COMMIT"
        exit 0
    fi
    echo "valkey at $current — fetching $COMMIT"
    git -C "$DEST" fetch --depth 1 origin "$COMMIT"
    git -C "$DEST" checkout "$COMMIT"
    exit 0
fi

mkdir -p "$(dirname "$DEST")"
git clone "$REPO" "$DEST"
git -C "$DEST" checkout "$COMMIT"
echo "Cloned valkey at $COMMIT"
