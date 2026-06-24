#!/usr/bin/env bash
set -euo pipefail

HOST="${1:-127.0.0.1}"
PORT="${2:-7432}"
CONCURRENCY="${3:-10}"
ROWS_PER_CLIENT="${4:-1000}"

PSQL_BASE="psql -h $HOST -p $PORT -U postgres -d postgres -v ON_ERROR_STOP=1 -q"

echo "=== oigrap load test ==="
echo "Concurrency: $CONCURRENCY clients, $ROWS_PER_CLIENT rows each"

# Setup: create table
$PSQL_BASE -c "DROP TABLE IF EXISTS load_test_t;"
$PSQL_BASE -c "CREATE TABLE load_test_t (client_id INTEGER, row_id INTEGER, val TEXT);"

START=$(date +%s%3N)

# Launch N parallel clients, each inserting ROWS_PER_CLIENT rows
pids=()
for c in $(seq 1 $CONCURRENCY); do
    (
        for r in $(seq 1 $ROWS_PER_CLIENT); do
            $PSQL_BASE -c "INSERT INTO load_test_t VALUES ($c, $r, 'data_${c}_${r}');" > /dev/null
        done
    ) &
    pids+=($!)
done

# Wait for all clients
for pid in "${pids[@]}"; do
    wait "$pid"
done

END=$(date +%s%3N)
ELAPSED=$(( END - START ))

TOTAL=$(( CONCURRENCY * ROWS_PER_CLIENT ))
echo "Inserted $TOTAL rows in ${ELAPSED}ms"
echo "Throughput: $(( TOTAL * 1000 / ELAPSED )) rows/sec"

# Verify count
COUNT=$($PSQL_BASE -t -c "SELECT COUNT(*) FROM load_test_t;" | tr -d ' ')
echo "Verified row count: $COUNT / $TOTAL"

if [ "$COUNT" != "$TOTAL" ]; then
    echo "FAIL: row count mismatch"
    exit 1
fi

# Read benchmark: scan all rows
SCAN_START=$(date +%s%3N)
$PSQL_BASE -c "SELECT COUNT(*), MAX(row_id) FROM load_test_t;" > /dev/null
SCAN_END=$(date +%s%3N)
echo "Full table scan: $(( SCAN_END - SCAN_START ))ms"

# Join benchmark: self-join on client_id
JOIN_START=$(date +%s%3N)
$PSQL_BASE -c "SELECT COUNT(*) FROM load_test_t a JOIN load_test_t b ON a.client_id = b.client_id WHERE a.row_id = 1;" > /dev/null
JOIN_END=$(date +%s%3N)
echo "Self-join scan: $(( JOIN_END - JOIN_START ))ms"

# Cleanup
$PSQL_BASE -c "DROP TABLE load_test_t;"

echo ""
echo "=== Load test complete ==="
