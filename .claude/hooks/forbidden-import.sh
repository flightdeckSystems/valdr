#!/usr/bin/env bash
set -uo pipefail
export PORT_PROJECT_ROOT="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/../.." && pwd)}"
export PORT_HARNESS_DIR="${PORT_HARNESS_DIR:-$(dirname "$PORT_PROJECT_ROOT")/port-harness}"
export PORT_FORBIDDEN_PATTERNS_SH="${PORT_FORBIDDEN_PATTERNS_SH:-$PORT_PROJECT_ROOT/harness/forbidden-patterns.sh}"
exec "$PORT_HARNESS_DIR/hooks/forbidden-pattern.sh"
