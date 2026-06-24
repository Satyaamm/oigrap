# oigrap — Overview

## Mission

Build a single database engine that makes the five-system modern data stack unnecessary. Not by bolting models together at the API layer, but by unifying them at the physical storage layer under one query planner, one transaction model, and one deployment binary.

---

## The Name

oigrap. Internal project name, no public meaning assigned. The product identity is separate from the implementation codename.

---

## Core Design Principles

### 1. Unification at the physical layer

Every existing multi-model database is actually one database with adapters. SurrealDB is a document store that added graph syntax. MongoDB is a document store that added SQL-ish queries. ArangoDB is a document store with a graph API bolted on.

oigrap is designed so that the same 8KB page can participate in row scan, columnar aggregation, and vector ANN recall. The storage engine does not know "this table is relational" or "this table is a document store." Every table is a collection of pages. The query planner decides how to physically access them.

### 2. No frameworks, no SDKs, no shortcuts

The storage engine, query parser, optimizer, execution engine, vector index, wire protocol, and distributed consensus are all written from scratch. This is a deliberate choice, not stubbornness.

Building from first principles means:
- No upstream API breakage
- No license contamination
- Deep understanding of every subsystem (required to extend and optimize)
- No performance ceiling imposed by a framework's abstraction layer

The only external dependencies allowed are: compression codecs (LZ4, Zstd), TLS implementation, and CRC32. These are utilities, not database logic.

### 3. PostgreSQL wire protocol

Users connect with psql, any PostgreSQL driver, any ORM that supports Postgres, any BI tool. Zero new tooling required. The wire protocol is the user's interface. The internal engine can be completely novel while the external surface is entirely familiar.

### 4. Single binary, embedded first

The first deployable artifact is a single binary with no external dependencies. Run it like SQLite. This is how DuckDB and SQLite achieved massive adoption. The embedded mode is hardened before distributed complexity is added. A production-ready single-node database is shipped before RAFT is written.

### 5. One query language

Standard SQL extended with three syntax additions for non-relational workloads. Nothing proprietary to learn:

```sql
-- Vector: ORDER BY ... <-> ... LIMIT n  (distance operator)
-- Document: data->>'key', data @> '{"k":"v"}'  (PostgreSQL JSON syntax)
-- Graph: WITH RECURSIVE  (standard SQL, optimized execution)
```

A developer who knows PostgreSQL knows oigrap on day one.

### 6. ACID across all storage layouts

A single write transaction that touches a row store table, a vector column, and a JSON document is atomic, consistent, isolated, and durable. No eventual consistency escape hatches. No "vector writes are async." One MVCC implementation covers all physical storage types.

---

## What oigrap is not

- Not a data warehouse (it handles OLAP but that is one of many workloads)
- Not a vector database (vector is one index type among several)
- Not a graph database (graph traversal is one query pattern the optimizer understands)
- Not a document database (document storage is one physical layout option)
- Not a relational database (relational is one physical layout option)

oigrap is a storage and query engine that happens to do all of those things under one roof.

---

## Key design decisions

| Decision | Choice | Reason |
|----------|--------|--------|
| Primary language | Rust | No GC pauses in storage hot paths, compile-time memory safety, zero-cost abstractions |
| Wire protocol language | Go | Network I/O, connection multiplexing, strong concurrency primitives |
| Page size | 8KB default, configurable | Matches OS page size, aligns with SSD sector size |
| Storage model | Slotted pages + heap file | Proven design (PostgreSQL), well-understood modification costs |
| Transaction isolation | Snapshot isolation as default, SSI as strong mode | SI handles 95% of workloads without serialization overhead |
| Columnar format | Custom, Apache Arrow-aligned layout | Zero-copy interop potential, SIMD-friendly alignment |
| Vector index | HNSW (then DiskANN for large datasets) | HNSW is in-memory and fast to prototype; DiskANN is production-scale |
| Distributed consensus | RAFT | Understandable, well-specified, multiple reference implementations to validate against |
| Join ordering | Dynamic programming (Selinger 1979) | Optimal for up to ~10 tables, the gold standard |

---

## The query that defines the project

This query should run natively with no ETL, no cross-system joins, no manual orchestration:

```sql
SELECT
    u.name,
    d.title,
    d.embedding <-> u.interest_vector AS relevance,
    u.data->>'plan' AS subscription_plan
FROM users u
JOIN documents d ON d.author_id = u.id
WHERE
    u.data->>'plan' = 'enterprise'
    AND d.published_at > NOW() - INTERVAL '30 days'
ORDER BY relevance
LIMIT 20;
```

This query touches:
- Row store (users, documents — relational join on author_id)
- Vector index (embedding <-> interest_vector — ANN distance)
- Document/JSON (data->>'plan' — dynamic schema field)

One query. One plan. One system. No pipelines.

---

## System overview diagram

```
Client (psql, any PG driver, any ORM)
           |
           | PostgreSQL wire protocol v3 (TCP)
           |
    +--------------+
    | Wire Layer   |  Connection manager, protocol state machine,
    | (Go)         |  session context, prepared statement cache
    +--------------+
           |
    +--------------+
    | SQL Parser   |  Lexer -> Tokens -> Recursive descent parser
    |              |  -> AST -> Query rewriter
    +--------------+
           |
    +--------------+
    | Query        |  Logical plan -> Rule-based rewrites
    | Optimizer    |  -> Statistics -> DP join ordering
    |              |  -> Cost model (I/O + CPU + ANN + graph)
    |              |  -> Physical plan
    +--------------+
           |
    +--------------+
    | Execution    |  Vectorized operators on columnar batches
    | Engine       |  HashJoin, MergeSort, Aggregate, Filter,
    |              |  Scan, VectorScan, GraphExpand
    +--------------+
           |
    +------+-------+----------+
    |              |           |
+--------+   +----------+  +---------+
| Row    |   | Columnar |  | Vector  |
| Store  |   | Store    |  | Index   |
| (OLTP) |   | (OLAP)   |  | (HNSW)  |
+--------+   +----------+  +---------+
    |              |           |
    +------+-------+----------+
           |
    +--------------+
    | Storage      |  Buffer pool, WAL, heap files,
    | Engine       |  B+ tree indexes, page manager,
    | (Rust)       |  free space map, visibility map
    +--------------+
           |
    +--------------+
    | Transaction  |  MVCC, XID assignment, snapshot
    | Manager      |  management, deadlock detection,
    |              |  lock manager, vacuum
    +--------------+
           |
    +--------------+
    | Disk         |  OS file I/O, fsync, mmap,
    |              |  direct I/O for columnar paths
    +--------------+
```
