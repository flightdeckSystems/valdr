#!/usr/bin/env bash
set -uo pipefail
export PORT_PROJECT_ROOT="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/../.." && pwd)}"
export PORT_HARNESS_DIR="${PORT_HARNESS_DIR:-$(dirname "$PORT_PROJECT_ROOT")/port-harness}"
export PORT_UNSAFE_BUDGETS_TOML="${PORT_UNSAFE_BUDGETS_TOML:-$PORT_PROJECT_ROOT/harness/unsafe-budgets.toml}"
exec "$PORT_HARNESS_DIR/hooks/unsafe-budget.sh"
