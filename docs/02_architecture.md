# Architecture

## System overview

oigrap is a layered system. Each layer has a single responsibility. Layers communicate through defined interfaces. No layer reaches past its immediate neighbor.

```
+------------------------------------------------------------------+
|                         CLIENT LAYER                             |
|  psql  |  libpq  |  JDBC  |  node-postgres  |  asyncpg  |  etc  |
+------------------------------------------------------------------+
                              |
                     TCP connection
                              |
+------------------------------------------------------------------+
|                       WIRE LAYER  (Go)                           |
|                                                                  |
|  Connection Manager    Session Context    Auth Handler           |
|  Protocol State Machine    Prepared Statement Cache              |
|  Error Formatter    Notice Sender    Cancel Handler              |
+------------------------------------------------------------------+
                              |
                    Internal query struct
                              |
+------------------------------------------------------------------+
|                      PARSER LAYER  (Rust)                        |
|                                                                  |
|  Lexer (tokenizer)    Parser (recursive descent)                 |
|  AST builder          Query rewriter (view expansion, etc.)      |
+------------------------------------------------------------------+
                              |
                    AST / logical plan
                              |
+------------------------------------------------------------------+
|                    OPTIMIZER LAYER  (Rust)                       |
|                                                                  |
|  Logical Planner       Rule-Based Rewriter                       |
|  Statistics Manager    Cost Model                                |
|  DP Join Ordering      Physical Plan Generator                   |
|  ANN Cost Estimator    Graph Traversal Cost Estimator            |
+------------------------------------------------------------------+
                              |
                    Physical plan
                              |
+------------------------------------------------------------------+
|                   EXECUTION ENGINE  (Rust)                       |
|                                                                  |
|  Operator Tree (vectorized, batch-oriented)                      |
|                                                                  |
|  SeqScan | IndexScan | VectorScan | GraphExpand                  |
|  HashJoin | MergeJoin | NestedLoop                               |
|  HashAggregate | StreamAggregate                                 |
|  Sort | Limit | Filter | Project | Insert | Update | Delete      |
+------------------------------------------------------------------+
                              |
              +---------------+---------------+
              |               |               |
+-------------+  +------------+  +------------+
| ROW STORE   |  | COLUMNAR   |  | VECTOR     |
| (Rust)      |  | STORE      |  | INDEX      |
|             |  | (Rust)     |  | (Rust)     |
| Slotted     |  | Column     |  | HNSW       |
| pages       |  | segments   |  | graph      |
| Heap file   |  | Compressed |  | DiskANN    |
| B+ tree idx |  | encoded    |  | (at scale) |
+-------------+  +------------+  +------------+
              |               |               |
              +---------------+---------------+
                              |
+------------------------------------------------------------------+
|                    STORAGE ENGINE  (Rust)                        |
|                                                                  |
|  Buffer Pool Manager      Page Manager                           |
|  Write-Ahead Log (WAL)    Heap File Manager                      |
|  Free Space Map           Visibility Map                         |
|  B+ Tree Implementation   Checkpointer                           |
+------------------------------------------------------------------+
                              |
+------------------------------------------------------------------+
|                  TRANSACTION MANAGER  (Rust)                     |
|                                                                  |
|  XID Manager              Snapshot Manager                       |
|  Lock Manager             MVCC Visibility                        |
|  Deadlock Detector        Vacuum                                 |
+------------------------------------------------------------------+
                              |
+------------------------------------------------------------------+
|                   DISTRIBUTED LAYER  (Rust)                      |
|            (Phase 2 — not in initial implementation)             |
|                                                                  |
|  RAFT Consensus           Partition Router                       |
|  Log Replication          Snapshot Transfer                      |
|  Membership Manager       Distributed Query Coordinator          |
+------------------------------------------------------------------+
                              |
                          OS / Disk
```

---

## Component responsibilities

### Wire Layer (Go)

The wire layer speaks PostgreSQL frontend/backend protocol v3 over TCP. It is the only component written in Go. Every other component is Rust.

Responsibilities:
- Accept TCP connections
- Perform SSL negotiation (optional)
- Handle PostgreSQL startup handshake and authentication
- Maintain session state: current database, current user, transaction state, prepared statements
- Parse incoming protocol messages: Query, Parse, Bind, Execute, Sync, Describe, Close
- Route query strings to the parser layer
- Route results back to the client in correct wire format
- Handle cancel requests (out-of-band UDP cancel)
- Format error messages in PostgreSQL error response format (severity, code, message, detail, hint)

The wire layer does not touch storage. It does not interpret SQL. It is a protocol translator between TCP bytes and internal query structs.

### Parser Layer (Rust)

Converts a SQL string into a typed Abstract Syntax Tree (AST). Two stages:

**Lexer**: converts the raw string into a flat list of typed tokens. No tree structure. Token types include keywords (SELECT, FROM, WHERE, JOIN, INSERT, CREATE, DROP, etc.), identifiers, string literals, numeric literals, operators (<->, @>, ->>, etc.), punctuation.

**Parser**: recursive descent parser that consumes the token stream and builds an AST. Each grammar rule is a function. The parser handles operator precedence via a Pratt parser for expressions.

**Query rewriter**: transforms the AST before planning. Rewrites views into their underlying query. Flattens uncorrelated subqueries. Normalizes boolean expressions. Generates implicit casts.

### Optimizer Layer (Rust)

Takes the AST and produces a physical execution plan. Three phases:

**Logical planning**: convert AST into a logical plan tree. Logical plan nodes are abstract (LogicalScan, LogicalFilter, LogicalJoin, LogicalAggregate). They describe what to compute, not how.

**Rule-based rewriting**: apply algebraic transformations that are always beneficial. Predicate pushdown (move WHERE filters as close to the scan as possible). Projection pruning (eliminate columns not referenced). Constant folding. Expression simplification.

**Cost-based optimization**: enumerate physical implementations using dynamic programming. For each logical plan node, generate candidate physical operators. For joins: hash join, merge join, nested loop join. For scans: sequential scan, index scan, bitmap scan, vector scan. Estimate the cost of each candidate using the cost model. Select the minimum-cost physical plan.

The cost model must understand four workload types simultaneously:
- Row scan cost: pages * page_read_cost + tuples * tuple_process_cost
- Columnar scan cost: columns * column_decompression_cost + batch_process_cost
- ANN search cost: f(index_size, ef_search, vector_dimension, recall_target)
- Graph traversal cost: f(fan_out, depth, edge_count, selectivity)

### Execution Engine (Rust)

Executes the physical plan as a tree of operators. Uses vectorized (batch-oriented) execution: operators exchange columnar batches of 1024–8192 rows rather than processing one row at a time. This keeps data in CPU cache across operator calls and enables SIMD.

Operator interface:

```rust
trait Operator {
    fn schema(&self) -> Schema;
    fn next_batch(&mut self) -> Option<Batch>;
    fn close(&mut self);
}
```

Every operator implements `next_batch()`. The root operator is called by the execution engine. It calls its children. Children call their children. A Batch is a columnar buffer: a Vec of column arrays, each column array being a contiguous memory region of typed values.

### Row Store (Rust)

Physical storage for OLTP data and document data. Data is organized as heap files of slotted pages. Each page is 8KB. Each page contains a variable number of tuples. Tuples are addressed by (page_id, slot_id) pairs called TupleIDs (TIDs).

The row store is the primary layout. When a table is created without specifying a layout hint, it defaults to row storage.

B+ tree indexes are maintained separately from heap data. An index entry maps a key value to a TID. Index scans produce TIDs, which are then used to fetch tuples from the heap.

### Columnar Store (Rust)

Physical storage for OLAP data. Data is organized as column segments: each column of a table is stored as a separate file of fixed-size blocks. Within a block, values are tightly packed, optionally compressed with delta encoding, run-length encoding, or dictionary encoding.

A table can be stored in row format, columnar format, or both simultaneously (the dual-format case). The query optimizer chooses which physical layout to access based on query shape: column-selective analytical queries hit columnar segments; point-lookup queries hit row pages.

### Vector Index (Rust)

Stores and searches high-dimensional float vectors. The index structure is HNSW (Hierarchical Navigable Small World graph).

HNSW stores vectors as nodes in a multilayer graph. Nodes are connected to their nearest neighbors at each layer. Search enters at the top (sparser) layer and descends greedily toward the query vector, refining the candidate set as it descends.

The vector index is integrated into the optimizer's cost model. An ANN search is treated as an index scan with a configurable recall parameter. The optimizer can push predicates into the vector scan or apply them as a post-filter depending on selectivity estimates.

### Storage Engine (Rust)

The foundation of everything. Manages:

**Buffer pool**: a fixed-size pool of memory frames. Pages are loaded from disk into frames. The buffer pool tracks which pages are in memory, which are dirty, and which frames are currently pinned by active operations.

**WAL (Write-Ahead Log)**: a sequential append-only file on disk. Every modification to a data page is recorded in the WAL before the page is written to disk. On crash, the WAL is replayed to restore a consistent state. The WAL is the durability guarantee.

**Page manager**: allocates, reads, writes, and frees 8KB pages on disk. Maintains a free page list.

**Checkpointer**: periodically flushes dirty pages from the buffer pool to disk and writes a checkpoint record to the WAL. Checkpoints bound recovery time: after a checkpoint, WAL records before the checkpoint are not needed for recovery.

### Transaction Manager (Rust)

Manages concurrent access from multiple sessions.

**XID Manager**: assigns a monotonically increasing transaction ID (XID) to each transaction. XIDs are 64-bit integers.

**MVCC**: each tuple in the heap has xmin (the XID that created it) and xmax (the XID that deleted it, or 0 if live). A transaction reading a tuple applies visibility rules: the tuple is visible if xmin is committed and precedes the reader's snapshot, and xmax is either 0 or aborted or after the reader's snapshot. This allows readers and writers to proceed concurrently without blocking each other.

**Lock Manager**: handles explicit locking (SELECT FOR UPDATE, DDL locks). Row-level locks are stored in a hash table keyed by TID. Table-level locks use a separate table. Lock acquisition checks for conflicts with existing holders. Deadlock is detected by cycle detection in the wait-for graph.

**Vacuum**: reclaims space occupied by tuples that are no longer visible to any active transaction (dead tuples). Vacuum scans heap pages, identifies dead tuples, and marks their space as reusable.

---

## Data flow: write path

```
Client sends: INSERT INTO users (id, name) VALUES (1, 'Alice')
                    |
         Wire layer receives Query message
                    |
         Parser: SQL string -> InsertStmt AST
                    |
         Optimizer: InsertStmt -> physical Insert plan
         (no join ordering needed, trivial plan)
                    |
         Executor: begins transaction (assigns XID)
                    |
         Executor: finds free page in heap file (via FSM)
                    |
         Executor: pins page in buffer pool
                    |
         Executor: writes tuple into page slot
         Tuple header: xmin=current_xid, xmax=0, cid=command_id
                    |
         Storage engine: writes WAL record:
         [LSN][XID][INSERT][page_id][slot_id][tuple_data]
                    |
         Executor: marks page dirty in buffer pool
                    |
         Client sends: COMMIT
                    |
         Transaction manager: writes COMMIT WAL record
                    |
         Transaction manager: WAL flush to disk (fdatasync)
                    |
         Client receives: CommandComplete
```

The page is now dirty in the buffer pool. It will be written to disk later by the checkpointer or when the buffer pool needs to evict the frame. The data is durable because the WAL is flushed.

---

## Data flow: read path

```
Client sends: SELECT name FROM users WHERE id = 1
                    |
         Wire layer receives Query message
                    |
         Parser: SQL string -> SelectStmt AST
                    |
         Optimizer:
           - Checks statistics: users table has B+ tree index on id
           - Cost of index scan << cost of sequential scan
           - Physical plan: IndexScan(users_id_idx, id=1) -> Project(name)
                    |
         Executor: begins read-only transaction (takes snapshot)
         Snapshot: {xmin=100, xmax=205, active_xids=[201,203]}
                    |
         Executor: IndexScan looks up id=1 in B+ tree
         B+ tree returns: TID (page_id=7, slot_id=3)
                    |
         Executor: pins page 7 in buffer pool
         (if not in pool: reads from disk, loads into free frame)
                    |
         Executor: reads tuple at slot 3
         Applies MVCC visibility: xmin=100 (committed, before snapshot)
                                  xmax=0 (tuple is live)
         Tuple is visible
                    |
         Executor: Project extracts 'name' column
                    |
         Wire layer: encodes result as RowDescription + DataRow
                    |
         Client receives: Alice
```

---

## Key interface contracts

### Buffer pool interface

```rust
trait BufferPool {
    fn fetch_page(&mut self, page_id: PageId) -> &Page;
    fn new_page(&mut self) -> (PageId, &mut Page);
    fn mark_dirty(&mut self, page_id: PageId);
    fn pin(&mut self, page_id: PageId);
    fn unpin(&mut self, page_id: PageId, dirty: bool);
    fn flush_page(&mut self, page_id: PageId);
}
```

### Operator interface

```rust
trait Operator: Send {
    fn schema(&self) -> &Schema;
    fn next_batch(&mut self, ctx: &ExecContext) -> Result<Option<Batch>>;
    fn close(&mut self);
}
```

### Transaction interface

```rust
trait TransactionManager {
    fn begin(&mut self) -> Transaction;
    fn commit(&mut self, txn: Transaction) -> Result<()>;
    fn abort(&mut self, txn: Transaction);
    fn snapshot(&self, txn: &Transaction) -> Snapshot;
    fn is_visible(&self, tuple_header: &TupleHeader, snap: &Snapshot) -> bool;
}
```

---

## Non-goals (out of scope for initial implementation)

- Full-text search (FTS). Defer to later.
- Geospatial indexes. Defer to later.
- Stored procedures beyond simple UDFs.
- Logical replication (publishing changes as a stream to external consumers).
- Online schema changes (ALTER TABLE without locking). Ship locking version first.
- Multi-tenancy isolation below the database level.
- Query result caching.
