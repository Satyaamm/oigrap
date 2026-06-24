# oigrap

A unified database engine built from scratch.

One engine. One query language. One transaction model. All data workloads.

---

## What it replaces

Companies today run five separate systems for five data workloads:

- Postgres — transactional row data (OLTP)
- ClickHouse — analytics over columnar data (OLAP)
- Pinecone / Weaviate — vector similarity search
- MongoDB — documents and flexible schema
- Neo4j — graph relationships and traversals

Five systems means five sync pipelines, five failure domains, five ops teams, and an impossible query boundary between them. You cannot join your analytics data with your vector embeddings without exporting, transforming, and loading between systems.

oigrap eliminates all five systems with one engine that handles every workload natively at the physical storage layer — not as adapters or wrappers, but as first-class storage layouts under a single query planner.

---

## Core thesis

The unification must happen at the storage layer, not the API layer.

Every existing "multi-model" database bolts models on top of each other: a document store with a graph API on top, a relational engine with a vector plugin. These systems always betray their origin model — graph queries fall back to row scans, vector searches can't participate in join optimization, document updates bypass ACID guarantees.

oigrap is designed from page zero with three physical storage layouts — row, columnar, vector — sharing one buffer pool, one WAL, one MVCC transaction manager, and one query optimizer that understands all workload costs simultaneously.

---

## Documentation

| Document | Contents |
|----------|----------|
| [Overview](docs/00_overview.md) | Mission, principles, design decisions |
| [Problem Statement](docs/01_problem.md) | Why this exists, market gap, user pain |
| [Architecture](docs/02_architecture.md) | Full system diagram, component map, data flow |
| [Storage Engine](docs/03_storage_engine.md) | Buffer pool, WAL, pages, heap file, recovery |
| [Transaction Manager](docs/04_mvcc.md) | MVCC, isolation levels, deadlock detection |
| [SQL Parser](docs/05_sql_parser.md) | Lexer, parser, AST, query rewriting |
| [Query Optimizer](docs/06_query_optimizer.md) | Logical plan, DP optimizer, cost model |
| [Execution Engine](docs/07_execution_engine.md) | Vectorized operators, Arrow format, SIMD |
| [Storage Layouts](docs/08_storage_layouts.md) | Row store, columnar store, document store |
| [Vector Index](docs/09_vector_index.md) | HNSW algorithm, ANN search, cost integration |
| [Graph Engine](docs/10_graph_engine.md) | Adjacency storage, traversal, path algorithms |
| [Wire Protocol](docs/11_wire_protocol.md) | PostgreSQL wire protocol implementation |
| [Distributed Layer](docs/12_distributed.md) | RAFT consensus, sharding, replication |
| [Data Formats](docs/13_data_formats.md) | Binary page format, WAL format, encodings |
| [Build Roadmap](docs/14_build_roadmap.md) | Phase-by-phase plan with concrete milestones |
| [Research References](docs/15_research_refs.md) | Foundational papers |

---

## Language

- Storage engine, execution engine: Rust
- Wire protocol, API layer: Go
- In-memory columnar format: Apache Arrow layout (implemented from scratch)
- Vector index algorithm: HNSW (implemented from scratch)

No prebuilt database SDKs. No query engine frameworks. Everything written from first principles.

---

## Status

Pre-implementation. Documentation phase.
