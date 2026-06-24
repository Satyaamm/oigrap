# Research References

The foundational papers behind every design decision in oigrap. Read these before implementing the relevant component.

---

## Storage Engine

**Architecture of a Database System**
Hellerstein, Stonebraker, Hamilton (2007)
Foundations and Trends in Databases, Vol. 1, No. 2
The most comprehensive overview of how a relational database is structured. Covers buffer management, storage management, query processing, and transactions. Read this first.

**The Design and Implementation of a Log-Structured File System**
Rosenblum & Ousterhout (1992)
ACM Transactions on Computer Systems
Original LSM tree concept. Relevant for understanding why sequential writes are fast and random writes are slow, even on SSDs.

**ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead Logging**
Mohan, Haderle, Lindsay, Pirahesh, Schwarz (1992)
ACM Transactions on Database Systems
The definitive WAL and recovery algorithm. oigrap implements ARIES. Read sections 3, 4, and 5 carefully.

**LRU-K: Improving Buffer Management Using Historical Information**
O'Neil, O'Neil, Weikum (1993)
Proceedings of ACM SIGMOD
The LRU-K replacement policy used in oigrap's buffer pool.

---

## MVCC and Transactions

**A Critique of ANSI SQL Isolation Levels**
Berenson, Bernstein, Gray, Melton, O'Neil, O'Neil (1995)
Proceedings of ACM SIGMOD
Precise definitions of the SQL isolation anomalies (dirty read, non-repeatable read, phantom). Required reading before implementing any transaction system.

**Generalized Isolation Level Definitions**
Adya, Liskov, O'Neil (2000)
Proceedings of ICDE
Extends the Berenson critique with a formalization that handles MVCC correctly. The Berenson definitions were specified for 2PL systems.

**Serializable Isolation for Snapshot Databases**
Cahill, Rohm, Fekete (2008)
Proceedings of ACM SIGMOD
Describes Serializable Snapshot Isolation (SSI). oigrap uses SSI for its Serializable isolation level. This paper is the complete specification.

**Making Snapshot Isolation Serializable**
Fekete, Liarokapis, O'Neil, O'Neil, Shasha (2005)
ACM Transactions on Database Systems
The theoretical basis for SSI. Read alongside the Cahill 2008 paper.

---

## Query Optimization

**Access Path Selection in a Relational Database Management System**
Selinger, Astrahan, Chamberlin, Lorie, Price (1979)
Proceedings of ACM SIGMOD
The original dynamic programming join optimizer. The algorithm oigrap implements is directly descended from this paper.

**How Good Are Query Optimizers, Really?**
Leis, Gubichev, Mirchev, Boncz, Kemper, Neumann (2015)
Proceedings of VLDB
Evaluates the quality of cardinality estimates in real optimizers. Shows that bad estimates are a major source of bad plans. Guides the statistics design.

**Eddies: Continuously Adaptive Query Processing**
Avnur & Hellerstein (2000)
Proceedings of ACM SIGMOD
Adaptive query processing. Not directly implemented in oigrap but useful for understanding the limits of static optimization.

**Volcano — An Extensible and Parallel Query Evaluation System**
Graefe (1994)
IEEE Transactions on Knowledge and Data Engineering
The Volcano (iterator) model that forms the basis of most query executors. oigrap extends this to vectorized execution.

---

## Vectorized Execution and Columnar Storage

**MonetDB/X100: Hyper-Pipelining Query Execution**
Boncz, Zukowski, Nes (2005)
Proceedings of CIDR
The original vectorized execution paper. Describes the column-at-a-time processing model that makes analytical queries fast on modern CPUs.

**Column-Stores vs. Row-Stores: How Different Are They Really?**
Abadi, Madden, Hachem (2008)
Proceedings of ACM SIGMOD
Empirical comparison of column-store and row-store performance on analytical workloads. Identifies the specific optimizations that make column stores fast: late materialization, column-specific compression, vectorized processing.

**Efficient Data Compression in a Column-Store DBMS**
Abadi, Madden, Ferreira (2006)
Proceedings of ICDE
Column-specific encoding schemes: RLE, dictionary encoding, bit packing. Directly informs oigrap's columnar encoding design.

**Rethinking SIMD Vectorization for In-Memory Databases**
Polychroniou, Raghavan, Ross (2015)
Proceedings of ACM SIGMOD
How to use SIMD instructions effectively for database operations. Covers selection, aggregation, and hash join.

**Morsel-Driven Parallelism: A NUMA-Aware Query Evaluation Framework**
Leis, Boncz, Kemper, Neumann (2014)
Proceedings of ACM SIGMOD
How HyPer parallelizes query execution across NUMA machines. Relevant for future parallel execution in oigrap.

---

## Vector Indexing

**Efficient and Robust Approximate Nearest Neighbor Search Using Hierarchical Navigable Small World Graphs**
Malkov & Yashunin (2018)
IEEE Transactions on Pattern Analysis and Machine Intelligence
The HNSW paper. Read it completely. The algorithm description in section 3 is the direct basis for oigrap's implementation.

**DiskANN: Fast Accurate Billion-Point Nearest Neighbor Search on a Single Node**
Subramanya, Devvrit, Simhadri, Krishnawamy, Kadekodi (2019)
NeurIPS 2019
Disk-based ANN search for datasets too large for RAM. oigrap plans DiskANN for its Phase 2 vector scaling.

**Faiss: A Library for Efficient Similarity Search**
Johnson, Douze, Jegou (2017)
arXiv:1702.08734
Meta's vector similarity search library. Useful as a reference for SIMD-optimized distance computation, even though oigrap implements its own.

**ANN Benchmarks: A Benchmarking Tool for Approximate Nearest Neighbor Algorithms**
Aumüller, Bernhardsson, Faithfull (2020)
Information Systems
The standard benchmark methodology for ANN indexes. oigrap's vector index must be evaluated against this benchmark.

---

## Graph Processing

**LSQB: A Large-Scale Subgraph Query Benchmark**
Mhedhbi, Gupta, Khaliq, Salihoglu (2021)
Proceedings of GRADES-NDA
Graph query benchmarks relevant for evaluating oigrap's graph traversal performance.

**Piuma: Taming Graph Processing with Sparse Dynamic Computations**
Fern, et al. (2021)
arXiv
Modern graph processing on hardware. Background reading.

**GraphScope: A Unified Engine for Big Graph Processing**
Fan, et al. (2021)
Proceedings of VLDB
How a production system handles diverse graph workloads in one engine.

---

## Distributed Systems

**In Search of an Understandable Consensus Algorithm**
Ongaro & Ousterhout (2014)
Proceedings of USENIX ATC
The RAFT paper. Read it completely. The algorithm in section 5 is what oigrap implements. The extended version (Ongaro's PhD thesis) has additional details on cluster membership changes.

**Spanner: Google's Globally Distributed Database**
Corbett, et al. (2012)
Proceedings of OSDI
Google's production distributed database. Relevant for understanding TrueTime and external consistency. oigrap does not need TrueTime but the architecture is instructive.

**F1: A Distributed SQL Database That Scales**
Shute, et al. (2013)
Proceedings of VLDB
Google's distributed SQL layer on top of Spanner. Relevant for understanding distributed query execution.

**Calvin: Fast Distributed Transactions for Partitioned Database Systems**
Thomson, Diamond, Weng, Ren, Shao, Abadi (2012)
Proceedings of ACM SIGMOD
Alternative to 2PC for distributed transactions. Background reading for the distributed layer.

---

## PostgreSQL Wire Protocol

**PostgreSQL Frontend/Backend Protocol**
PostgreSQL Global Development Group
https://www.postgresql.org/docs/current/protocol.html
The authoritative specification. Required reading before implementing the wire layer.

---

## B+ Trees

**The Ubiquitous B-Tree**
Comer (1979)
ACM Computing Surveys
The original comprehensive survey of B-trees. Covers all variants including B+ trees.

**Efficient Locking for Concurrent Operations on B-Trees**
Lehman & Yao (1981)
ACM Transactions on Database Systems
The B-link tree: allows concurrent operations with minimal locking. oigrap's B+ tree implementation uses B-link tree locking.

---

## Reading order for implementers

Phase 1 (storage): Architecture of a Database System -> ARIES
Phase 2 (transactions): Berenson et al. (isolation levels) -> Cahill et al. (SSI)
Phase 3 (query): Selinger 1979 -> Graefe (Volcano)
Phase 4 (vectorized): Boncz et al. (MonetDB/X100) -> Abadi et al. (column stores)
Phase 5 (vector index): Malkov & Yashunin (HNSW)
Phase 6 (distributed): Ongaro & Ousterhout (RAFT)
