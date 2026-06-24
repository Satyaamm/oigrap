#!/usr/bin/env bash
set -euo pipefail

HOST="${1:-127.0.0.1}"
PORT="${2:-7432}"

DIR="$(cd "$(dirname "$0")" && pwd)"

echo "Running oigrap test suite against $HOST:$PORT"
echo ""

bash "$DIR/smoke_test.sh" "$HOST" "$PORT"
echo ""
bash "$DIR/edge_case_test.sh" "$HOST" "$PORT"
echo ""
bash "$DIR/load_test.sh" "$HOST" "$PORT" 5 200
echo ""
echo "All tests complete."
