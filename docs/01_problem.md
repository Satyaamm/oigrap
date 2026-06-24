# Problem Statement

## The five-system data stack

Every modern application beyond a certain scale runs multiple databases simultaneously. Not by choice — by necessity. Each database type was designed for one workload and degrades significantly outside that workload.

```
Application
    |
    +-- Postgres       (user accounts, orders, inventory — OLTP)
    |       |
    |       +-- sync pipeline (Debezium / Kafka)
    |
    +-- ClickHouse     (analytics, dashboards, aggregations — OLAP)
    |       |
    |       +-- sync pipeline (Airbyte / dbt)
    |
    +-- Pinecone       (semantic search, recommendations — vector)
    |       |
    |       +-- sync pipeline (custom ETL)
    |
    +-- MongoDB        (user-generated content, events, logs — documents)
    |       |
    |       +-- sync pipeline (Kafka Connect)
    |
    +-- Neo4j          (social graph, recommendations, lineage — graph)
```

This is the real production stack at companies with more than ~50k users. It is not a bad choice — it is the only viable option given current tooling.

---

## The costs of polyglot persistence

### Operational cost

Five databases means five teams (or one team stretched thin across five unfamiliar systems), five monitoring setups, five backup strategies, five on-call runbooks, five upgrade cycles. The operational overhead is not additive — it is multiplicative because failures compose.

### Sync pipeline cost

Data that lives in Postgres must be replicated to ClickHouse for analytics queries. Data that is inserted into Postgres must have its embedding computed and synced to Pinecone. Data in MongoDB must be synced to Neo4j when a relationship edge is created.

Each pipeline is a failure surface: it lags, it duplicates, it drops rows on schema changes, it falls behind during high write load, and it introduces consistency windows that are invisible to the application.

A user's profile update in Postgres takes 3-50ms. The same update reaching ClickHouse for analytics might take 30 seconds to 30 minutes depending on pipeline configuration. This means dashboards show stale data. This is considered "normal."

### The query boundary problem

This is the fundamental technical problem. You cannot write a SQL query that joins Postgres data with Pinecone data. The boundary between systems is a hard wall.

The workaround is application-level orchestration:

```python
# What developers actually write today
user = postgres.query("SELECT * FROM users WHERE id = ?", user_id)
interest_vector = user["interest_vector"]
similar_docs = pinecone.query(vector=interest_vector, top_k=20)
doc_ids = [d.id for d in similar_docs]
docs = postgres.query("SELECT * FROM documents WHERE id = ANY(?)", doc_ids)
analytics = clickhouse.query("SELECT views FROM doc_stats WHERE doc_id = ANY(?)", doc_ids)
# now manually merge results in Python
```

This is five round trips, four systems, and result merging done in application code with no optimizer. The database's query optimizer — the most sophisticated piece of the system — is bypassed entirely. The developer is manually writing what the optimizer should do automatically.

### Consistency across systems

ACID transactions do not span system boundaries. A Postgres transaction and a Pinecone write are two independent operations. If the Postgres commit succeeds and the Pinecone write fails, the systems are inconsistent. Every application that writes to multiple databases lives with this risk.

The standard answer is "eventual consistency and idempotent writes." This is not a solution. It is an acknowledgment that the problem is unsolved.

---

## Why existing solutions fail

### Approach 1: Multi-model databases (SurrealDB, ArangoDB, FaunaDB)

These databases correctly identify the problem but solve it at the wrong layer. They build multiple data models on top of a single underlying storage engine, but the underlying engine is always one of the models:

- SurrealDB: document store underneath, graph and relational as query transformations on top
- ArangoDB: document store underneath (RocksDB), graph as adjacency lists in documents
- FaunaDB: temporal document store, relational queries compiled to document scans

The result: good enough for simple use cases, but graph queries in SurrealDB are sequential document scans. Analytical aggregations in ArangoDB are full document deserializations. Vector search in any of these systems is a post-filter on top of document retrieval, not a first-class query operator.

### Approach 2: Unified query layer (Presto, Trino, Starburst)

These systems provide a single SQL interface over multiple underlying databases. Presto can query Postgres and ClickHouse and S3 in one SQL statement.

But: they do not solve the storage problem. Data still lives in separate systems. The query planner cannot optimize across system boundaries (it cannot use statistics from Postgres to inform a ClickHouse scan). Every query still crosses network boundaries. Transactions are impossible. These are read-only federation layers, not a unified database.

### Approach 3: Lakehouse (Databricks, Snowflake)

Correct for analytics at scale. Wrong for operational (OLTP) workloads. You cannot run a user-facing application that writes to Snowflake. Latency is in seconds, not milliseconds. No row-level locking. No sub-millisecond point lookups. Vector search is bolted on (Databricks Vector Search is an approximate external index). Graph is not supported.

### Approach 4: NewSQL (CockroachDB, TiDB, Spanner)

Correct for distributed OLTP. Wrong for analytics, vector, graph, and document. CockroachDB is PostgreSQL-compatible and horizontally scalable — but it is a row store. Running aggregation queries on CockroachDB with millions of rows is significantly slower than ClickHouse on the same data. They have no vector index. They have no graph traversal optimization. They are excellent relational databases that solve the distributed OLTP problem only.

---

## The market gap

The gap is not "a multi-model database." The gap is:

> A database where the query planner has a unified cost model that simultaneously understands row scan costs, columnar aggregation costs, ANN recall-accuracy tradeoffs, and graph traversal fan-out — and can optimize across all of them in one physical plan.

No existing production system has this. It requires:
1. A custom storage engine that physically stores all layouts
2. A custom query optimizer that models all layout costs
3. A custom execution engine with operators for all workload types
4. A custom transaction manager that wraps all layouts in MVCC

This cannot be assembled from existing components. It must be built.

---

## Why now

Three forces converge in 2024-2026 that make this the right time:

**1. Vector search became mainstream**
AI application development exploded with LLMs. Every application now needs to store and query embeddings. Pinecone, Weaviate, Milvus grew from niche tools to infrastructure requirements. The market now expects vector search to be a database primitive, not a separate system.

**2. The operational cost of polyglot persistence became visible**
As companies matured their data infrastructure, the sync pipeline maintenance burden became undeniable. Engineering teams now have dedicated "data platform" or "data infrastructure" teams whose primary job is maintaining pipelines between databases. This is pure overhead.

**3. The hardware has caught up**
NVMe SSDs changed the I/O cost model fundamentally. Random reads that were prohibitive on spinning disk are fast on NVMe. This opens up new storage engine designs that were not viable before 2020. DiskANN (the production-scale vector index) only makes sense with NVMe latency characteristics.

---

## The user oigrap is built for

Primary: Backend engineers and data engineers at companies running 2+ database systems who feel the sync pipeline pain and the query boundary pain daily.

Secondary: Infrastructure teams who want to reduce the number of systems they operate.

Tertiary: AI application developers who need to store embeddings alongside structured data without managing a separate vector database.

The user does not want to learn a new query language. They know SQL. They want SQL to work across all their data without pipelines.
