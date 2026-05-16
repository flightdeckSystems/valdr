#!/usr/bin/env bash
set -uo pipefail
export PORT_PROJECT_ROOT="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/../.." && pwd)}"
export PORT_HARNESS_DIR="${PORT_HARNESS_DIR:-$(dirname "$PORT_PROJECT_ROOT")/port-harness}"
export PORT_TYPE_VOCAB_SCANNER="${PORT_TYPE_VOCAB_SCANNER:-$PORT_PROJECT_ROOT/harness/check_type_vocabulary.py}"
exec "$PORT_HARNESS_DIR/hooks/type-vocabulary-scan.sh"
