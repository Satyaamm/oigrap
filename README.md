# oigrap

A multi-model database engine written from scratch in Rust. Row storage, columnar OLAP, vector ANN, JSONB, graph, and RAFT consensus unified under one SQL interface with full ACID guarantees and serializable isolation. Connects on port **7432** via the PostgreSQL wire protocol — any psql-compatible client works out of the box.

## Why oigrap

Every production database stack eventually bolts together multiple specialized systems: a relational database for transactions, a vector database for search, a graph database for relationships, a document store for flexible schemas. Each system has its own connection pool, its own consistency model, its own operational footprint. Queries that join data across systems require application-side glue.

oigrap runs all six storage models under one SQL parser, one transaction log, and one port. A single query can join a row-store table with a vector search result and a JSONB document without leaving the database.

```sql
SELECT p.title, p.author, 1 - (d.embedding <-> $1) AS score
FROM docs d
JOIN posts p ON p.id = d.post_id
WHERE d.metadata @> '{"published": true}'
ORDER BY d.embedding <-> $1
LIMIT 10;
```

## Engines

| Engine | What it provides |
|---|---|
| Row store | Slotted 8 KB heap pages, buffer pool with LRU eviction, B+ tree secondary indexes, 2PL lock manager with DFS deadlock detection |
| Columnar OLAP | RLE, dictionary, and delta encoding per column; zone-map pruning; vectorized GROUP BY without full-row materialization |
| Vector ANN | HNSW multi-layer graph index (M=16, ef\_construction=200); greedy beam search with select-neighbors heuristic; L2 distance; recall >0.80 at k=10 |
| JSONB | 8-tag binary encoding (0x01–0x08); GIN posting-list index; `->` `->>` `@>` `?` `\|\|` `#>` operators |
| Graph | `oigrap_shortest_path()` BFS; `oigrap_pagerank()` 20-iteration power iteration (damping 0.85); `WITH RECURSIVE` with cycle detection and `DEPTH()` |
| Distributed | RAFT consensus (election 150–300 ms, heartbeat 50 ms); range-partitioned shard router; two-phase commit |

All engines share:

- **WAL** — 34-byte ARIES-style record header (LSN, prev\_LSN, xid, rmgr, type, length, CRC32). 4 MB in-memory buffer, auto-flush at 2 MB. Full redo + undo recovery.
- **MVCC** — Snapshot isolation with serializable snapshot isolation (SSI) via rw-anti-dependency cycle detection.
- **Wire protocol** — PostgreSQL Frontend/Backend Protocol v3. Simple and extended query. Trust and MD5 auth. TLS via rustls with ephemeral self-signed cert. Full pg\_catalog shim for ORMs and GUI tools.

## Quick start

**Docker:**

```bash
docker pull ghcr.io/satyaamm/oigrap:latest
docker run -p 7432:7432 ghcr.io/satyaamm/oigrap:latest
psql -h localhost -p 7432 -U postgres
```

**Build from source:**

```bash
git clone https://github.com/Satyaamm/oigrap.git
cd oigrap
cargo build --release
./target/release/oigrap 0.0.0.0:7432
```

Requirements: Rust 1.78+, no external database dependencies.

**First queries:**

```sql
-- Row store
CREATE TABLE users (id INTEGER, name TEXT);
INSERT INTO users VALUES (1, 'alice'), (2, 'bob');
SELECT * FROM users WHERE id = 1;

-- JSONB
CREATE TABLE events (id INTEGER, payload TEXT);
INSERT INTO events VALUES (1, '{"type":"login","user":{"id":42}}');
SELECT payload->>'type', payload->'user'->>'id' FROM events;
SELECT * FROM events WHERE payload @> '{"type":"login"}';

-- Vector ANN
CREATE TABLE docs (id INTEGER, content TEXT, embedding TEXT);
INSERT INTO docs VALUES (1, 'hello world', '[0.1, 0.2, 0.3]');
SELECT content FROM docs ORDER BY embedding <-> '[0.15, 0.25, 0.35]' LIMIT 5;

-- Graph
CREATE TABLE follows (src BIGINT, dst BIGINT);
INSERT INTO follows VALUES (1,2),(2,3),(3,4);
SELECT oigrap_shortest_path('follows', 1, 4);   -- returns 3
SELECT oigrap_pagerank('follows', 3);
```

## Architecture

```
PostgreSQL wire protocol (port 7432)
              |
        SQL parser  (hand-written, no parser generator)
              |
        Query planner  (Selinger DP for n<=7, greedy for n>7)
              |
           Executor
    /    /    |    \    \    \
 Row  Col  Vec  JSONB Graph Dist
 store stor ANN  GIN         (RAFT
 heap  col  HNSW posting     + 2PC)
              \    \    /    /
               WAL (ARIES, 34-byte header)
                       |
               MVCC / SSI (rw-anti-dependency)
                       |
               8 KB slotted pages (fdatasync)
```

## Workspace

```
crates/
  storage/    Page manager, buffer pool, disk manager, WAL, MVCC, HNSW, columnar, JSONB, GIN
  sql/        Parser, AST, planner, executor, spill-to-disk hash join (FNV-1a, 100k threshold)
  server/     PostgreSQL wire protocol, pg_catalog shim, TLS (rustls + rcgen)
  raft/       RAFT consensus, shard router (binary search), two-phase commit
```

## Configuration

```
oigrap [OPTIONS] [BIND_ADDR]

  BIND_ADDR                   Listen address (default: 0.0.0.0:7432)
  --data-dir <PATH>            Data directory (default: ./data)
  --pool-size <N>              Buffer pool frames (default: 1024)
  --wal-buffer <BYTES>         WAL buffer size (default: 4194304)
  --shard-id <ID>              Shard ID for distributed mode
  --join <ADDR>                Peer address to join existing cluster

Environment variables:
  OIGRAP_DATA_DIR              Same as --data-dir
  OIGRAP_POOL_SIZE             Same as --pool-size
  HASH_JOIN_SPILL_THRESHOLD    Row count before spilling hash join to disk (default: 100000)
```

## Tests

```bash
cargo test                    # 207 unit tests, 0 failures
./scripts/smoke_test.sh       # end-to-end: DDL, DML, joins, JSONB, vectors, graph
./scripts/load_test.sh        # concurrent writers + readers under load
./scripts/edge_case_test.sh   # nulls, type coercion, transaction isolation, spill
./scripts/run_all_tests.sh    # runs all of the above
```

## Client compatibility

Any PostgreSQL-wire-protocol client connects. Verified:

| Client | Notes |
|---|---|
| psql | Full simple + extended query |
| psycopg2 / psycopg3 | |
| node-postgres | |
| pgx (Go) | |
| SQLAlchemy | pg\_catalog shim handles ORM introspection |
| DBeaver, TablePlus, DataGrip | PostgreSQL connection type, port 7432 |
| Metabase, Grafana | PostgreSQL data source plugin |

SCRAM-SHA-256 is not implemented. Configure clients to use MD5 or trust (password-less) auth.
Use `sslmode=disable` or set clients to accept self-signed certificates.

## Contributing

Pull requests are welcome. All PRs require maintainer review and approval before merge. Open an issue first for significant changes.

1. Fork the repository and create a branch off `main`
2. Add tests for new behaviour
3. Run `cargo test` and `cargo clippy -- -D warnings` — both must pass with zero warnings
4. Open a pull request against `main`

## Status

Alpha. Storage formats and wire protocol are mostly stable but may change before 1.0. Not recommended for production use without thorough testing in your environment.

## License

MIT
