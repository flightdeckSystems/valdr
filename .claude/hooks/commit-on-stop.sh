#!/usr/bin/env bash
set -uo pipefail
export PORT_PROJECT_ROOT="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/../.." && pwd)}"
export PORT_HARNESS_DIR="${PORT_HARNESS_DIR:-$(dirname "$PORT_PROJECT_ROOT")/port-harness}"
export PORT_GATING_HOOKS="$PORT_PROJECT_ROOT/.claude/hooks/unsafe-budget.sh:$PORT_PROJECT_ROOT/.claude/hooks/forbidden-import.sh:$PORT_PROJECT_ROOT/.claude/hooks/type-vocabulary.sh:$PORT_PROJECT_ROOT/.claude/hooks/trailer-required.sh"
exec "$PORT_HARNESS_DIR/hooks/commit-on-stop.sh"
