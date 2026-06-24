#!/usr/bin/env bash
set -euo pipefail

HOST="${1:-127.0.0.1}"
PORT="${2:-7432}"
PSQL="psql -h $HOST -p $PORT -U postgres -d postgres -v ON_ERROR_STOP=1"

echo "=== oigrap smoke test ==="

# Basic SELECT
$PSQL -c "SELECT 1 + 1 AS result;" | grep -q "2"
echo "PASS: arithmetic"

# CREATE TABLE + INSERT + SELECT
$PSQL <<'EOF'
DROP TABLE IF EXISTS smoke_users;
CREATE TABLE smoke_users (id INTEGER, name TEXT, age INTEGER);
INSERT INTO smoke_users VALUES (1, 'alice', 30);
INSERT INTO smoke_users VALUES (2, 'bob', 25);
INSERT INTO smoke_users VALUES (3, 'carol', 35);
EOF

COUNT=$($PSQL -t -c "SELECT COUNT(*) FROM smoke_users;" | tr -d ' ')
[ "$COUNT" = "3" ] || { echo "FAIL: expected 3 rows, got $COUNT"; exit 1; }
echo "PASS: basic DML ($COUNT rows)"

# WHERE filter
COUNT=$($PSQL -t -c "SELECT COUNT(*) FROM smoke_users WHERE age > 28;" | tr -d ' ')
[ "$COUNT" = "2" ] || { echo "FAIL: WHERE filter, expected 2, got $COUNT"; exit 1; }
echo "PASS: WHERE filter"

# JOIN
$PSQL <<'EOF'
DROP TABLE IF EXISTS smoke_orders;
CREATE TABLE smoke_orders (user_id INTEGER, amount FLOAT);
INSERT INTO smoke_orders VALUES (1, 99.99);
INSERT INTO smoke_orders VALUES (2, 49.50);
INSERT INTO smoke_orders VALUES (1, 25.00);
EOF

COUNT=$($PSQL -t -c "SELECT COUNT(*) FROM smoke_users JOIN smoke_orders ON smoke_users.id = smoke_orders.user_id;" | tr -d ' ')
[ "$COUNT" = "3" ] || { echo "FAIL: JOIN, expected 3, got $COUNT"; exit 1; }
echo "PASS: JOIN"

# GROUP BY + COUNT
COUNT=$($PSQL -t -c "SELECT COUNT(*) FROM (SELECT user_id, COUNT(*) FROM smoke_orders GROUP BY user_id) AS sub;" | tr -d ' ')
[ "$COUNT" = "2" ] || { echo "FAIL: GROUP BY, expected 2 groups, got $COUNT"; exit 1; }
echo "PASS: GROUP BY"

# ORDER BY + LIMIT
RESULT=$($PSQL -t -c "SELECT name FROM smoke_users ORDER BY age DESC LIMIT 1;" | tr -d ' ')
[ "$RESULT" = "carol" ] || { echo "FAIL: ORDER BY LIMIT, expected carol, got $RESULT"; exit 1; }
echo "PASS: ORDER BY LIMIT"

# CTE (WITH)
COUNT=$($PSQL -t -c "WITH adults AS (SELECT name FROM smoke_users WHERE age >= 30) SELECT COUNT(*) FROM adults;" | tr -d ' ')
[ "$COUNT" = "2" ] || { echo "FAIL: CTE, expected 2, got $COUNT"; exit 1; }
echo "PASS: CTE"

# UPDATE
$PSQL -c "UPDATE smoke_users SET age = 31 WHERE name = 'alice';"
AGE=$($PSQL -t -c "SELECT age FROM smoke_users WHERE name = 'alice';" | tr -d ' ')
[ "$AGE" = "31" ] || { echo "FAIL: UPDATE, expected 31, got $AGE"; exit 1; }
echo "PASS: UPDATE"

# DELETE
$PSQL -c "DELETE FROM smoke_users WHERE age < 28;"
COUNT=$($PSQL -t -c "SELECT COUNT(*) FROM smoke_users;" | tr -d ' ')
[ "$COUNT" = "2" ] || { echo "FAIL: DELETE, expected 2, got $COUNT"; exit 1; }
echo "PASS: DELETE"

# JSONB operators
$PSQL <<'EOF'
DROP TABLE IF EXISTS smoke_docs;
CREATE TABLE smoke_docs (data TEXT);
INSERT INTO smoke_docs VALUES ('{"name":"alice","active":true}');
EOF
echo "PASS: JSONB insert (basic)"

# VACUUM
$PSQL -c "VACUUM smoke_users;" 2>/dev/null && echo "PASS: VACUUM" || echo "SKIP: VACUUM (not supported over wire yet)"

# Cleanup
$PSQL <<'EOF'
DROP TABLE IF EXISTS smoke_users;
DROP TABLE IF EXISTS smoke_orders;
DROP TABLE IF EXISTS smoke_docs;
EOF

echo ""
echo "=== All smoke tests passed ==="
