# Build Roadmap

This is the sequence in which oigrap gets built. The order is not arbitrary — each phase produces something testable and each phase's artifacts are required by the next.

The single mandatory principle: **nothing moves to the next phase until the current phase is correct and tested.** A production-ready storage engine before a production-ready optimizer. A production-ready optimizer before distribution.

---

## Phase 1: Storage Foundation (Months 1-3)

**Goal:** A database that can insert rows, scan them back, survive a crash, and recover correctly.

**No SQL. No network. No clients.** Just the storage layer, driven by Rust unit tests.

### Month 1: Buffer pool and page manager

Week 1-2: Buffer pool manager
- Fixed-size frame pool (configurable, default 128MB = 16384 frames of 8KB)
- Page table: HashMap<PageId, FrameId>
- Frame metadata: page_id, pin_count, is_dirty, last_access
- LRU eviction: eject the unpinned frame with oldest last_access
- Pin/unpin API
- Tests: fill pool, verify eviction, verify dirty pages are written before eviction

Week 3: Disk manager
- Single data file, O_DIRECT on Linux
- Allocate, read, write, free pages
- Page offset = page_id * PAGE_SIZE
- Free page list in page 0
- Tests: write page, read it back, allocate many pages, verify offset arithmetic

Week 4: Page layout (slotted pages)
- Page header encoding/decoding (see 13_data_formats.md)
- Slot array manipulation
- Tuple insert into page (write at `upper`, update slot, update `lower`)
- Tuple read from slot
- Free space calculation
- Tests: fill a page, read all tuples back, verify ordering

**Milestone: can write 10,000 tuples across multiple pages and read them all back correctly.**

### Month 2: WAL and heap file

Week 5-6: Write-Ahead Log
- WAL file: append-only, sequential
- WAL buffer (in-memory ring buffer)
- Write HEAP_INSERT, HEAP_UPDATE, HEAP_DELETE records
- Flush WAL buffer to disk (fdatasync)
- LSN assignment (monotonic counter)
- Tests: write 1000 WAL records, read them back, verify LSNs are monotonic

Week 7: Heap file manager
- Table as collection of pages
- Free Space Map: tree of uint8 values, O(log n) lookup of page with N free bytes
- Insert tuple: find page in FSM, write to page, update FSM
- Scan: sequential page-by-page iteration
- Tests: insert 100,000 tuples, sequential scan returns all of them

Week 8: WAL enforcement
- "WAL before data" rule: mark page dirty only after WAL record is flushed
- LSN in page header: page's LSN = LSN of most recent WAL record affecting it
- Buffer pool: before flushing dirty page, assert WAL is flushed to at least page LSN
- Tests: kill the process mid-write, verify WAL file is consistent

**Milestone: insert rows, scan them back. Kill the process, restart, WAL is readable.**

### Month 3: Recovery and transactions

Week 9-10: ARIES recovery
- Analysis phase: scan WAL from last checkpoint, build txn table and dirty page table
- Redo phase: reapply all WAL records since redo_lsn
- Undo phase: reverse all incomplete transactions (write CLRs)
- Tests: insert rows, commit, insert more rows, crash (SIGKILL), recover, verify only committed rows are present

Week 11: Transaction manager and MVCC
- XID manager: atomic counter for next XID
- Tuple headers with xmin, xmax, cid
- Snapshot: {xmin, xmax, active_xids}
- Visibility function: is_tuple_visible(tuple_header, snapshot)
- Tests: two concurrent "transactions" (simulated), verify isolation

Week 12: Checkpointer
- Background thread that flushes dirty pages periodically
- Writes CHECKPOINT WAL record with redo_lsn and active txn table
- Tests: run 10,000 transactions, checkpoint, crash, recover in <5 seconds

**Milestone: SIGKILL the process at any point, restart, all committed data is present, no uncommitted data is visible. Recovery completes in under 5 seconds for 100MB of WAL.**

---

## Phase 2: B+ Tree and Basic SQL (Months 4-5)

**Goal:** Point lookups by primary key. Basic SQL execution. No network yet.

### Month 4: B+ tree

Week 13-14: B+ tree structure
- Node pages: internal nodes (key -> child_page) and leaf nodes (key -> TID)
- Insert: find leaf, add entry, split if overflow
- Lookup: descend from root, binary search at each level
- Range scan: find left boundary leaf, follow right-sibling pointers
- Tests: insert 1M entries, verify all lookups correct, range scans correct

Week 15-16: B+ tree integration with heap
- Primary key index: maintained automatically on INSERT, UPDATE, DELETE
- Index scan operator: lookup key in B+ tree -> get TID -> fetch heap tuple
- Update: delete old index entry, insert new index entry
- Tests: insert, update, delete via primary key, verify index consistency

**Milestone: `INSERT INTO users (id, name) VALUES (1, 'Alice')` and `SELECT name FROM users WHERE id = 1` work (as Rust API calls, not SQL strings).**

### Month 5: SQL parser and basic executor

Week 17-18: SQL lexer + parser
- Lexer: all token types (see 05_sql_parser.md)
- Recursive descent parser for SELECT, INSERT, UPDATE, DELETE, CREATE TABLE, DROP TABLE
- AST construction
- Tests: parse 100 sample SQL queries, verify AST structure

Week 19-20: Basic query executor (row-at-a-time, no optimizer)
- Logical planner: AST -> logical plan
- Naive executor: SeqScan, Filter, Project, Insert, Delete
- No join ordering. No cost model. Just make it work.
- Tests: execute basic SELECT, INSERT, UPDATE, DELETE

**Milestone: `SELECT name FROM users WHERE age > 25` executes correctly via SQL string input.**

---

## Phase 3: Wire Protocol (Month 6)

**Goal:** psql can connect and execute queries.

Week 21-22: PostgreSQL wire protocol (Go)
- TCP server
- Startup/authentication flow
- Simple query protocol: Query -> RowDescription -> DataRow* -> CommandComplete -> ReadyForQuery
- Error response format
- Tests: connect with psql, execute `SELECT 1`, receive result

Week 23-24: Extended query protocol
- Parse -> Bind -> Execute -> Sync
- Prepared statements cache
- Portal management
- Tests: prepared statement with parameters, execute 1000 times

**Milestone: `psql -h localhost -p 5432 -d oigrap` works. `CREATE TABLE`, `INSERT`, `SELECT` all work from the psql prompt.**

---

## Phase 4: Query Optimizer (Months 7-8)

**Goal:** The query planner chooses efficient access paths. Joins work correctly and efficiently.

### Month 7: Statistics and cost model

Week 25-26: Statistics collection (ANALYZE)
- Per-table: row count, page count, avg row size
- Per-column: NDV, null fraction, MCV list, histogram (100 buckets)
- ANALYZE command: scan table, compute statistics, write to pg_statistic
- Tests: verify statistics accuracy on known datasets

Week 27-28: Selectivity estimation and scan cost model
- Selectivity for equality, range, IN, IS NULL predicates
- SeqScan cost formula
- IndexScan cost formula
- Choose between SeqScan and IndexScan based on selectivity
- Tests: optimizer chooses IndexScan for `WHERE id = 1`, SeqScan for `WHERE age > 0`

### Month 8: Join ordering and physical plan

Week 29-30: Hash join implementation
- Build phase: inner table -> hash table (handle spill to disk)
- Probe phase: outer table -> probe hash table -> emit matches
- Left outer join: emit unmatched outer rows with NULLs
- Tests: join two 100,000-row tables, verify correctness

Week 31-32: DP join ordering
- Logical to physical plan conversion
- DP algorithm for join ordering (Selinger)
- Hash join cost estimation
- EXPLAIN output
- Tests: 5-table join, verify optimizer selects close-to-optimal plan

**Milestone: 5-table join runs within 2x of hand-optimized order. EXPLAIN shows correct plan.**

---

## Phase 5: Vectorized Execution and Columnar Store (Months 9-11)

**Goal:** Analytical queries run at ClickHouse-competitive speeds.

### Month 9: Vectorized execution

Week 33-36: Batch-oriented execution
- Batch type (columnar, see 07_execution_engine.md)
- Vectorized operators: SeqScan, Filter, Project, HashAggregate, Sort, TopK
- Expression evaluation on columns (tight loops, no per-row function calls)
- Tests: aggregate 10M rows, compare speed to row-at-a-time baseline

### Months 10-11: Columnar storage

Week 37-40: Column segment format
- Writer: encode values with Plain/RLE/Delta/Dictionary encoding, compress with LZ4
- Reader: decompress, decode, return as typed arrays
- Segment statistics: min, max, null count
- Zone map pruning: skip segments that cannot satisfy predicate

Week 41-44: Columnar scan operator
- ColumnarScan: read column segments, apply vectorized filter, return batches
- Predicate pushdown into segment reader
- CREATE TABLE ... STORAGE COLUMNAR
- Tests: 100M row analytical query, measure I/O and CPU utilization

**Milestone: `SELECT COUNT(*), AVG(amount) FROM orders GROUP BY category` over 100M rows runs in under 10 seconds on commodity hardware.**

---

## Phase 6: Vector Index (Month 12)

**Goal:** Vector similarity search works and integrates with SQL.

Week 45-48: HNSW implementation
- Node storage: all vectors in contiguous memory
- Layer graph: neighbor lists per node per layer
- Insert algorithm with heuristic neighbor selection
- Search algorithm with ef_search parameter
- Persistence: HNSW file format (see 13_data_formats.md)
- Vector distance syntax: `embedding <-> '[...]'`
- VectorScan physical plan node
- Integration with optimizer cost model

**Milestone: `SELECT id FROM documents ORDER BY embedding <-> query_vec LIMIT 10` returns correct nearest neighbors at > 0.95 recall, in < 10ms for 1M vectors.**

---

## Phase 7: Document / JSON (Month 13)

Week 49-52: JSONB support
- JSONB binary format: fast key lookup without parsing
- JSON operators: ->, ->>, @>, ?, ||, - (see 08_storage_layouts.md)
- GIN index on JSONB columns
- `data_type = JSONB` in CREATE TABLE

**Milestone: `SELECT data->>'name' FROM users WHERE data @> '{"plan":"enterprise"}'` works with GIN index.**

---

## Phase 8: Graph Traversal (Month 14)

Week 53-56: Recursive CTE execution
- WITH RECURSIVE query parsing and planning
- RecursiveScan operator: frontier-based BFS iteration
- Cycle detection via visited set
- Depth limiting
- Graph-optimized index usage (src, label) composite index

**Milestone: 5-hop graph traversal on 1M node graph completes in < 5 seconds.**

---

## Phase 9: Distributed Layer (Months 15-20)

**Goal:** oigrap runs as a cluster of nodes, tolerates node failures.

Month 15-16: RAFT consensus
- Leader election, log replication, heartbeats
- Snapshot and log compaction
- Tests: 3-node cluster, kill leader, verify new leader elected, no data loss

Month 17-18: Sharding
- Hash partitioning
- Shard router
- Distributed query coordinator
- Cross-shard aggregation

Month 19-20: Two-phase commit
- Cross-shard transactions
- Coordinator failure recovery
- Read replicas with configurable consistency

**Milestone: 3-node cluster handles 10,000 writes/second, survives node failure with < 500ms interruption.**

---

## Testing strategy (applies to every phase)

**Unit tests:** Every function that can be tested in isolation has a unit test. Storage engine functions are tested by writing and reading known byte sequences. Parser functions are tested with known SQL strings and expected ASTs.

**Property tests (fuzzing):** Use `proptest` (Rust) to generate random valid inputs and verify invariants hold. Key invariants to fuzz:
- Buffer pool: all pins are released, dirty pages are written before eviction
- WAL: all records are readable after any write pattern
- MVCC: committed data is always visible, uncommitted data is never visible
- HNSW: ANN search recall >= threshold for all random query vectors

**Crash tests:** For every write path, inject crashes at every WAL write, page flush, and fsync. Verify the database recovers correctly every time. This is the most important test for the storage engine.

**Integration tests:** After Phase 3, every new feature has a SQL-level integration test: write SQL, execute via the wire protocol, verify results.

**Benchmarks:** After Phase 5, maintain TPC-H benchmark results. After Phase 6, maintain ANN benchmark results (using the ann-benchmarks.com methodology). Regressions block merges.

---

## The first 30 days in detail

| Day | Task |
|-----|------|
| 1-3 | Set up Rust workspace, write page struct, encode/decode header |
| 4-7 | Buffer pool: frame pool, page table, pin/unpin, LRU eviction |
| 8-10 | Disk manager: open file, read/write pages, allocate/free |
| 11-14 | Slotted page: insert tuple, read tuple, free space tracking |
| 15-18 | WAL manager: append records, flush, LSN counter |
| 19-21 | WAL enforcement in buffer pool: pin page, write WAL, dirty page |
| 22-25 | Heap file: insert tuple end-to-end (FSM -> find page -> WAL -> write) |
| 26-28 | Heap scan: read all tuples from all pages of a table |
| 29-30 | Crash test: insert 10,000 rows, SIGKILL at random points, verify WAL is readable |

By day 30: a Rust program that inserts rows and scans them back, with a WAL that is always readable after a crash. The database is not usable yet. But the foundation is correct.
