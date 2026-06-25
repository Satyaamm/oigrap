# Changelog

All notable changes to oigrap will be documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Version numbers follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

Nothing yet.

## [0.1.0-alpha] - 2026-06-25

### Added

**Row store**
- Slotted 8 KB heap pages with 48-byte header (page_id, LSN, checksum, flags, lower/upper pointers)
- 24-byte tuple header layout: xmin, xmax, cid, infomask, infomask2 at exact byte offsets
- Buffer pool with LRU eviction (BTreeMap + HashMap + HashSet triple for O(log n) eviction)
- B+ tree secondary indexes with 28-byte special space layout
- 2PL lock manager with DFS deadlock detection; lock targets at Table and Tuple granularity
- Heap file with free-space map for efficient page allocation
- DiskManager with `MAGIC=b"OIGRAP\0\0"`, `FORMAT_VERSION=1`, positional I/O via `read_at`/`write_at`, fdatasync

**Write-ahead log (WAL)**
- ARIES-style 34-byte record header: LSN, prev_LSN, xid, rmgr_id, record_type, length, CRC32
- 4 MB in-memory buffer with automatic flush at 2 MB
- Six record types: HEAP_INSERT, HEAP_UPDATE, HEAP_DELETE, XACT_COMMIT, XACT_ABORT, CHECKPOINT
- Full three-phase ARIES recovery: Analysis, Redo (with LSN idempotency guard), Undo via prev_LSN chain

**MVCC and transactions**
- Snapshot isolation with Snapshot struct (xmin, xmax, active set)
- Serializable Snapshot Isolation (SSI) via rw-anti-dependency cycle detection on (table_id, page_id, slot_id) granularity
- Read and write set tracking per transaction; `check_ssi_conflict()` detects dangerous cycles before commit

**Columnar OLAP engine**
- Three column encoding types: RleColumn `Vec<(Value, u32)>`, DictColumn `Vec<Value>` + `Vec<u32>`, DeltaColumn `i64` + `Vec<i32>`
- Delta compression ratio: `(n*8) / (8 + (n-1)*4)` — approaches 2x for large n
- Zone maps with ZONE_SIZE=1000; prune_with_zone_maps skips zones using min/max bounds for =, !=, <, <=, >, >= predicates
- Vectorized aggregation pipeline: prune → batch_from_zones → eval_filter → filter → aggregate
- Binary persistence format with `COL\0` magic and per-value type tags

**Vector ANN engine (HNSW)**
- Hierarchical Navigable Small World index: M=16 (M0=32 at layer 0), ef_construction=200, level_mult=1/ln(16)≈0.361, max_level=16
- Layer assignment via PCG linear congruential generator: `level = floor(-ln(uniform) * level_mult)`
- Two-phase insert: search down to insertion layer, then insert with back-edge preservation pruning
- Beam search with dual MinEntry/HeapEntry heaps for candidate and result sets
- select_neighbors_heuristic for edge selection
- Binary persistence: `HNSW` magic + LE u32/f64 encoding
- CI recall guarantee: >0.80 at k=10 on 1,000 16-dimensional vectors with ef=50

**JSONB engine**
- 8-tag binary encoding: Object(0x01), Array(0x02), String(0x03), Int64(0x04), Float64(0x05), Bool(true=0x06, false=0x07), Null(0x08)
- Recursive-descent parser with `\uXXXX` unicode escape handling
- Operators: `->` (key as JSONB), `->>` (key as text), `@>` (containment), `?` (key existence), `||` (merge), `#>` (path navigation)
- GIN index: `BTreeMap<String, Vec<TupleId>>` posting lists, sorted for intersection using smallest-list-first strategy

**Graph engine**
- `oigrap_shortest_path(edge_table, src, dst)`: BFS over thread-local `EDGE_CACHE`, returns NULL if no path
- `oigrap_pagerank(edge_table, node)`: 20-iteration power iteration, damping factor 0.85, thread-local `PAGERANK_CACHE`
- `WITH RECURSIVE` CTEs with HashSet cycle detection
- `DEPTH()` function via thread-local `Cell<i64>`

**SQL engine**
- Hand-written recursive-descent parser; no parser generator dependency
- Selinger dynamic-programming join optimizer for n<=7 tables; greedy join order for n>7
- Spill-to-disk hash join: FNV-1a partitioning, spill threshold=100,000 rows, temp file cleanup via Drop
- Spill wire format: u32 col_count + per-value [u8 tag + payload] encoding
- Window functions with PARTITION BY and ORDER BY
- Common table expressions including recursive CTEs
- Full DDL (CREATE TABLE, CREATE INDEX, DROP TABLE), DML (INSERT, UPDATE, DELETE), SELECT with joins, GROUP BY, HAVING, ORDER BY, LIMIT/OFFSET

**Wire protocol**
- PostgreSQL Frontend/Backend Protocol v3, port 7432
- Simple query and extended query (Parse/Bind/Execute) protocols
- Trust and MD5 authentication (full RFC 1321 challenge-response)
- TLS via rustls with ephemeral self-signed certificate via rcgen
- pg_catalog shim: 14 virtual tables including pg_type, pg_namespace, pg_class, pg_tables, pg_attribute, information_schema.tables, information_schema.columns
- 20+ pg_catalog function stubs (version(), current_database(), current_schema(), etc.)
- SET/SHOW/DISCARD/DEALLOCATE statement handling

**Distributed engine**
- RAFT consensus: randomised election timeout 150–300 ms, heartbeat interval 50 ms
- Log replication with majority acknowledgement; log compaction with InstallSnapshot RPC
- TCP transport with 4-byte LE length-prefixed message framing, 1-second connect timeout
- Range-partitioned shard router with binary-search shard lookup; `fanout_query` + `merge_results`
- Two-phase commit coordinator (TwoPhaseRaftCoordinator) with AtomicU64 transaction counter

**Tests and tooling**
- 207 unit tests across all crates, 0 failures, 1 ignored
- `scripts/smoke_test.sh`, `load_test.sh`, `edge_case_test.sh`, `run_all_tests.sh`
- Dockerfile (multi-stage rust:1.78-slim → debian:bookworm-slim), docker-compose.yml with healthcheck

[Unreleased]: https://github.com/Satyaamm/oigrap/compare/v0.1.0-alpha...HEAD
[0.1.0-alpha]: https://github.com/Satyaamm/oigrap/releases/tag/v0.1.0-alpha
