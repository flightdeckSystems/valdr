#!/usr/bin/env bash
set -uo pipefail
export PORT_PROJECT_ROOT="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/../.." && pwd)}"
export PORT_HARNESS_DIR="${PORT_HARNESS_DIR:-$(dirname "$PORT_PROJECT_ROOT")/port-harness}"
export PORT_TYPE_VOCAB_TSV="${PORT_TYPE_VOCAB_TSV:-$PORT_PROJECT_ROOT/harness/type-vocabulary.tsv}"
exec "$PORT_HARNESS_DIR/hooks/pretooluse-vocab.sh"
