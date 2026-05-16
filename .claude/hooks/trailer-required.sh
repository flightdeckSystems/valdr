#!/usr/bin/env bash
set -uo pipefail
export PORT_PROJECT_ROOT="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/../.." && pwd)}"
export PORT_HARNESS_DIR="${PORT_HARNESS_DIR:-$(dirname "$PORT_PROJECT_ROOT")/port-harness}"
export PORT_TRAILER_FIELDS="${PORT_TRAILER_FIELDS:-source,target_crate,confidence,todos,port_notes,unsafe_blocks,notes}"
exec "$PORT_HARNESS_DIR/hooks/trailer-required.sh"
