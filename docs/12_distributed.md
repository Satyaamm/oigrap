# Distributed Layer

The distributed layer adds horizontal scalability and high availability to oigrap. A single-node oigrap handles datasets that fit on one machine. The distributed layer enables:
- Datasets larger than one node's storage
- Availability during node failures
- Higher read throughput via replicas

The distributed layer is Phase 2. The single-node implementation must be production-ready before distribution is added. Adding distribution to a buggy single-node database produces a buggy distributed database.

---

## Distributed consensus: RAFT

RAFT (Ongaro & Ousterhout, 2014) is the consensus algorithm oigrap uses for replication. RAFT manages a replicated log: a sequence of operations that all nodes agree upon. The database state is derived by applying the log entries in order.

RAFT was designed to be understandable. The paper explains every aspect of the algorithm clearly, making it the right choice for an implementation built from first principles.

### RAFT roles

Every node in a RAFT group is in one of three states:

**Leader**: receives client requests, appends to the log, replicates to followers. At any time there is at most one leader per RAFT group.

**Follower**: receives log entries from the leader, applies them to the state machine. Does not initiate anything.

**Candidate**: a follower that has not heard from the leader for a timeout period. Starts an election to become the new leader.

### RAFT terms

Time is divided into terms. Each term begins with an election. A term number is a monotonically increasing integer. If a node receives a message with a term higher than its own, it updates its term and reverts to follower state.

```rust
struct RaftState {
    // Persistent state (survives restart)
    current_term: u64,
    voted_for: Option<NodeId>,
    log: Vec<LogEntry>,

    // Volatile state
    commit_index: u64,      // highest log entry known to be committed
    last_applied: u64,      // highest log entry applied to state machine
    role: Role,             // Leader, Follower, Candidate
}

struct LogEntry {
    term:    u64,
    index:   u64,
    command: Command,  // the database operation to apply
}
```

### Leader election

A follower starts an election when its election timeout expires (randomized between 150-300ms — randomization prevents split votes):

```
Candidate sends RequestVote RPC to all other nodes:
  term:           candidate's current term
  candidate_id:   candidate's node ID
  last_log_index: index of candidate's last log entry
  last_log_term:  term of candidate's last log entry

Other node grants vote if:
  1. candidate's term >= node's current term
  2. node has not voted for another candidate in this term
  3. candidate's log is at least as up-to-date as node's log
     (compare last_log_term first, then last_log_index)

A candidate becomes leader when it receives votes from a majority (>n/2) of nodes.
```

### Log replication

Once elected, the leader replicates the log to followers:

```
Client write request arrives at leader.
Leader appends to its own log: LogEntry{term, index, command}
Leader sends AppendEntries RPC to all followers (simultaneously):
  term:           leader's current term
  leader_id:      leader's node ID
  prev_log_index: index of log entry immediately before new ones
  prev_log_term:  term of prev_log_index entry
  entries:        new log entries to append (may be empty = heartbeat)
  leader_commit:  leader's commit_index

Follower accepts if:
  1. term >= follower's current term
  2. Log contains entry at prev_log_index with term = prev_log_term
     (consistency check: follower's log matches leader's at this point)
If not consistent: follower rejects, leader decrements nextIndex and retries.

When a majority of nodes have appended the entry:
  Leader increments commit_index to the new entry's index
  Leader applies the command to its state machine
  Leader responds to client with success

Leader sends next heartbeat (AppendEntries with entries=[]):
  Includes updated commit_index
  Followers apply all log entries up to commit_index
```

### Log compaction (snapshots)

The log grows indefinitely. Snapshots compact the log: take a snapshot of the database state at a given log index, discard all log entries before that index.

```rust
struct Snapshot {
    last_included_index: u64,
    last_included_term:  u64,
    state: DatabaseSnapshot,  // complete database state at this log position
}
```

The snapshot is the complete database state (all tables, all indexes). After snapshotting, recovery from this snapshot + subsequent log entries is equivalent to replaying the entire log from the beginning.

For a database, the snapshot is essentially a copy of all data pages. This is expensive. oigrap uses **incremental snapshots**: instead of copying all data, record which pages have changed since the last snapshot and only copy the changed pages (similar to PostgreSQL's base backup with WAL streaming).

---

## Sharding

Sharding partitions data across multiple RAFT groups (shards). Each shard owns a subset of the data. The shard router directs queries to the correct shard(s).

### Partition schemes

**Hash partitioning**: hash the partition key, assign the row to shard `hash(key) % num_shards`.

```
Table: users, partition key: id
Shard 0: users where id % 4 = 0
Shard 1: users where id % 4 = 1
Shard 2: users where id % 4 = 2
Shard 3: users where id % 4 = 3
```

Advantages: uniform distribution. Disadvantage: range queries hit all shards.

**Range partitioning**: assign rows based on key ranges.

```
Table: orders, partition key: created_at
Shard 0: created_at < 2024-01-01
Shard 1: created_at in [2024-01-01, 2024-07-01)
Shard 2: created_at in [2024-07-01, 2025-01-01)
Shard 3: created_at >= 2025-01-01
```

Advantages: range queries hit one or few shards. Disadvantage: hot spots (recent data always hits the last shard).

**Consistent hashing**: nodes form a ring. Each node owns a range of the hash space. Adding/removing nodes only rebalances adjacent ranges.

Advantages: rebalancing when adding nodes moves minimal data. Disadvantage: more complex implementation.

oigrap starts with hash partitioning for simplicity. Range partitioning and consistent hashing are later additions.

### Shard router

```rust
struct ShardRouter {
    shard_map: Vec<ShardInfo>,   // shard_id -> RAFT group endpoints
    table_partitioning: HashMap<TableId, PartitionSpec>,
}

impl ShardRouter {
    fn route_write(&self, table: TableId, partition_key: &Value) -> ShardId;
    fn route_read(&self, table: TableId, predicate: &Expr) -> Vec<ShardId>;
    fn route_all(&self, table: TableId) -> Vec<ShardId>;  // for unpartitioned reads
}
```

### Distributed query execution

A query that spans multiple shards is broken into subqueries, one per shard. Results are collected at a coordinator node and merged.

```
Query: SELECT COUNT(*) FROM orders WHERE amount > 100
       (orders is sharded across 4 shards)

Coordinator:
  1. Send to shard 0: SELECT COUNT(*) FROM orders WHERE amount > 100
  2. Send to shard 1: SELECT COUNT(*) FROM orders WHERE amount > 100
  3. Send to shard 2: SELECT COUNT(*) FROM orders WHERE amount > 100
  4. Send to shard 3: SELECT COUNT(*) FROM orders WHERE amount > 100
  5. Collect results: [1200, 980, 1050, 1100]
  6. Merge: 1200 + 980 + 1050 + 1100 = 4330
  7. Return 4330 to client
```

For more complex queries (joins, aggregations), the coordinator generates a distributed physical plan:

```
Distributed plan for:
  SELECT u.name, SUM(o.amount)
  FROM users u JOIN orders o ON u.id = o.user_id
  GROUP BY u.name

  (users: sharded on id, orders: sharded on user_id — same key, co-located)

  Per shard (co-located join):
    HashAggregate(name, SUM(amount))
      HashJoin(users.id = orders.user_id)
        Scan(users partition)
        Scan(orders partition)

  Coordinator:
    MergeAggregate(sum the shard SUMs for each name)
      Collect results from all shards
```

Co-located joins (both tables sharded on the join key) execute entirely within each shard. Non-co-located joins require a shuffle: one table's data is redistributed by the join key to the shard holding the other table's matching rows (similar to MapReduce shuffle).

---

## Two-phase commit (2PC) for cross-shard transactions

A transaction that writes to multiple shards requires distributed coordination to maintain atomicity.

```
Phase 1: Prepare
  Coordinator sends Prepare(txn_id) to all participating shards.
  Each shard:
    1. Logs a PREPARE record to its WAL.
    2. Acquires all necessary locks (will hold until commit or abort).
    3. Responds: PREPARED (can commit) or ABORT (cannot commit).

Phase 2: Commit or Abort
  If all shards respond PREPARED:
    Coordinator logs COMMIT to its WAL (this is the commit point).
    Coordinator sends Commit(txn_id) to all shards.
    Each shard logs COMMIT, releases locks.
  If any shard responds ABORT:
    Coordinator sends Abort(txn_id) to all shards.
    Each shard logs ABORT, releases locks, undoes prepared changes.
```

2PC has a blocking problem: if the coordinator fails after Phase 1 but before Phase 2, participating shards are locked in the PREPARED state indefinitely. The solution: the coordinator is itself replicated via RAFT, so it can recover and complete Phase 2 after restart.

---

## Cluster topology

```
+-------------------+
|  Client / pgBouncer|
+-------------------+
         |
+-------------------+
|   Access Layer    |  -- Wire protocol, query parsing, routing
|   (any node)      |  -- Any node can serve reads/writes
+-------------------+
    |       |       |
+-------+ +-------+ +-------+
| Shard | | Shard | | Shard |
|  0    | |  1    | |  2    |
| (3    | | (3    | | (3    |
| nodes)| | nodes)| | nodes)|
+-------+ +-------+ +-------+
```

Each shard is a RAFT group of 3 or 5 nodes. 3 nodes tolerate 1 failure. 5 nodes tolerate 2 failures.

A dedicated metadata cluster (also RAFT-replicated) stores: table-to-shard mapping, schema, user configuration. All nodes query the metadata cluster on startup and cache the metadata locally.

---

## Read replicas

For read-heavy workloads, RAFT followers can serve read-only queries at the cost of potentially reading slightly stale data (log entries that the leader has committed but the follower has not yet applied). This is called follower reads.

For workloads requiring strong consistency on reads, oigrap uses leader reads: all reads go to the RAFT leader, which has the most up-to-date state.

Follower reads can be enabled per-session:

```sql
SET oigrap.read_consistency = 'eventual';   -- follower reads (lower latency)
SET oigrap.read_consistency = 'strong';     -- leader reads (always consistent, default)
```
