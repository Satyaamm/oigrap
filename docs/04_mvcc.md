# Transaction Manager and MVCC

Transactions are the mechanism by which concurrent database operations maintain consistency. Without transaction management, two sessions reading and writing the same data simultaneously produce incorrect results — lost updates, dirty reads, phantom rows.

oigrap implements Multi-Version Concurrency Control (MVCC). MVCC allows readers and writers to proceed concurrently without blocking each other. A reader sees a consistent snapshot of the database at the moment its transaction began, regardless of concurrent writes. A writer creates a new version of a tuple rather than overwriting the existing version.

---

## Transaction IDs (XID)

Every transaction is assigned a unique transaction ID (XID) when it begins. XIDs are monotonically increasing 64-bit unsigned integers. XID 0 is reserved (invalid). XID 1 is the bootstrap transaction that creates the initial schema.

```rust
struct XidManager {
    next_xid: AtomicU64,
    committed: BTreeSet<u64>,   // committed XIDs
    aborted: BTreeSet<u64>,     // aborted XIDs
    active: HashSet<u64>,       // currently active XIDs
}
```

XIDs are stored in tuple headers:
- `xmin`: the XID of the transaction that inserted this tuple version
- `xmax`: the XID of the transaction that deleted this tuple version (0 = not deleted)

---

## Snapshots

When a transaction begins (or at first query, depending on isolation level), it takes a snapshot. A snapshot captures the state of concurrent transactions at that moment:

```rust
struct Snapshot {
    xmin: u64,       // all transactions with XID < xmin are committed
    xmax: u64,       // all transactions with XID >= xmax are not yet started
    active: Vec<u64>, // transactions in progress when snapshot was taken
}
```

Tuple visibility rule: a tuple is visible to snapshot S if and only if:
1. `tuple.xmin < S.xmax` — the tuple was created before the snapshot horizon
2. `tuple.xmin` is committed AND `tuple.xmin NOT IN S.active` — the creating transaction committed before the snapshot
3. `tuple.xmax == 0` OR `tuple.xmax` is aborted OR `tuple.xmax IN S.active` OR `tuple.xmax >= S.xmax` — the tuple has not been deleted by a committed transaction visible to this snapshot

This rule gives each transaction a consistent point-in-time view of the database.

---

## MVCC tuple versioning

### Insert

Insert creates a new tuple with `xmin = current_xid`, `xmax = 0`. The tuple is immediately visible to the inserting transaction (its own XID is visible to itself). Other transactions see the tuple only after the inserting transaction commits.

### Update

Update in MVCC is a delete + insert. The old tuple version gets `xmax = current_xid`. A new tuple version is inserted with `xmin = current_xid`, `xmax = 0`. Both versions exist on disk simultaneously. Old readers continue to see the old version; new readers see the new version.

### Delete

Delete sets `xmax = current_xid` on the existing tuple. The tuple is not removed from the page. It remains until vacuum reclaims the space.

### Example

```
Timeline: XID 100 inserts row, XID 200 updates it, XID 300 deletes it

Page contents after all operations:
  Slot 1: (xmin=100, xmax=200, name='Alice')   -- original, deleted by 200
  Slot 2: (xmin=200, xmax=300, name='Alicia')  -- updated by 200, deleted by 300
  Slot 3: (xmin=300, xmax=0,   name='DELETED') -- this never exists; delete is xmax only

Transaction with snapshot {xmin=150, xmax=250, active=[210]}:
  Slot 1: xmin=100 < 150, committed, not in active -> visible start
           xmax=200 < 250, committed, not in active -> deleted
           RESULT: not visible (was deleted)

  Slot 2: xmin=200, 200 NOT < 150 -> not visible
           Wait: 200 < 250, not in active, committed
           Actually: xmin=200. Is 200 committed? Yes. Is 200 < xmax=250? Yes. Is 200 in active=[210]? No.
           So tuple visible. xmax=300 >= xmax=250 -> not deleted yet.
           RESULT: visible, value='Alicia'
```

---

## Isolation Levels

SQL defines four isolation levels. oigrap implements all four, with Snapshot Isolation as the practical default for Read Committed and Repeatable Read levels.

### Read Uncommitted

Not supported. oigrap never exposes uncommitted data. This isolation level is not useful for a serious database.

### Read Committed (default)

A transaction takes a fresh snapshot at the start of each SQL statement (not at the start of the transaction). This means within a multi-statement transaction, later statements see commits made by other transactions after the transaction began.

Anomalies possible: Non-repeatable reads (reading the same row twice in one transaction may return different values if another transaction committed between the reads).

Snapshot is refreshed per-statement:
```rust
fn execute_statement(&mut self, stmt: Statement) {
    if self.isolation_level == ReadCommitted {
        self.snapshot = self.snapshot_manager.take_snapshot();
    }
    // ... execute stmt using self.snapshot
}
```

### Repeatable Read

A transaction takes a snapshot once at the start of the first statement and uses it for all subsequent statements. Reads are repeatable — the same row will return the same value throughout the transaction.

Anomalies possible: Phantom reads (a WHERE clause that matched N rows initially may match N+M rows later if new rows were inserted and committed by another transaction). oigrap's snapshot isolation prevents most phantoms in practice because the snapshot captures which rows exist.

### Serializable (Serializable Snapshot Isolation — SSI)

The strongest isolation level. Serializable transactions produce results equivalent to some serial execution order. No anomalies.

oigrap implements SSI (Cahill et al., 2008). SSI runs on top of snapshot isolation and adds tracking of read/write conflicts between transactions. Three transactions T1, T2, T3 form a dangerous structure if: T1 reads data written by T2, T2 reads data written by T3, and T3 reads data written by T1 (a cycle in the dependency graph). SSI aborts one transaction in the cycle to prevent non-serializable execution.

SSI implementation requires:
- SIREAD locks: read locks that track which predicates each transaction has read (without blocking writers)
- Conflict tracking: record rw-antidependencies (T reads something T' later writes) and wr-dependencies (T writes something T' later reads)
- Cycle detection: periodically check for dangerous structures in the dependency graph

Abort rate under SSI is low for typical OLTP workloads. High-contention workloads may see higher abort rates.

---

## Lock Manager

Explicit locks (SELECT FOR UPDATE, DDL) and SSI read locks are managed by the lock manager.

### Lock modes

```
Row-level modes (weakest to strongest):
  FOR KEY SHARE     -- reading key columns
  FOR SHARE         -- reading all columns
  FOR NO KEY UPDATE -- updating non-key columns
  FOR UPDATE        -- updating any column, or deleting

Table-level modes:
  ACCESS SHARE      -- SELECT
  ROW SHARE         -- SELECT FOR UPDATE/SHARE
  ROW EXCLUSIVE     -- INSERT, UPDATE, DELETE
  SHARE UPDATE EXCLUSIVE -- VACUUM, ANALYZE
  SHARE             -- CREATE INDEX
  SHARE ROW EXCLUSIVE
  EXCLUSIVE
  ACCESS EXCLUSIVE  -- DROP TABLE, TRUNCATE, ALTER TABLE
```

### Compatibility matrix (row-level)

|                   | KEY SHARE | SHARE | NO KEY UPDATE | UPDATE |
|-------------------|-----------|-------|---------------|--------|
| KEY SHARE         | ok        | ok    | ok            | block  |
| SHARE             | ok        | ok    | block         | block  |
| NO KEY UPDATE     | ok        | block | block         | block  |
| UPDATE            | block     | block | block         | block  |

### Lock data structures

```rust
struct LockManager {
    row_locks: HashMap<TupleId, LockGroup>,
    table_locks: HashMap<TableId, LockGroup>,
    wait_graph: HashMap<XID, Vec<XID>>,  // who is waiting for whom
}

struct LockGroup {
    holders: Vec<LockHolder>,
    waiters: VecDeque<LockWaiter>,
}

struct LockHolder {
    xid: XID,
    mode: LockMode,
}
```

Lock acquisition:
1. Check if the requested lock mode is compatible with all current holders.
2. If compatible: add to holders, return immediately.
3. If not compatible: add to waiters queue. Suspend the requesting thread.
4. When a holder releases its lock: wake the first compatible waiter and grant its lock.

### Deadlock detection

A deadlock occurs when two or more transactions are each waiting for a lock held by the other. The wait-for graph contains a cycle.

Detection runs every 1 second (configurable). The detector builds the current wait-for graph from the lock manager state and runs DFS cycle detection. If a cycle is found, one transaction in the cycle is chosen as the victim (heuristic: youngest XID, lowest cost to abort) and aborted. Aborting releases all its locks, breaking the cycle.

---

## Vacuum

MVCC creates dead tuple versions that accumulate over time. Dead tuples waste space and slow sequential scans. Vacuum reclaims them.

### When a tuple is dead

A tuple with `xmax = X` is dead when transaction X is committed AND X is less than the oldest active transaction's XID (meaning no snapshot can possibly see the old version anymore). At this point, the space occupied by the tuple can be reclaimed.

### Vacuum algorithm

```
vacuum(table):
  oldest_xid = snapshot_manager.oldest_active_xid()
  for each page in table:
    fetch page
    for each slot in page:
      tuple = read_tuple(slot)
      if tuple.xmax != 0
         AND is_committed(tuple.xmax)
         AND tuple.xmax < oldest_xid:
           mark_slot_unused(slot)
           free_space += tuple.length
    update_fsm(page, free_space)
    unpin page
```

Vacuum does not hold locks on the table during its scan. It runs concurrently with regular operations. Vacuum only modifies pages to mark dead tuples unused; it does not reorder live tuples. A separate VACUUM FULL (which requires an exclusive lock) defragments pages and returns space to the OS.

### Autovacuum

The autovacuum daemon runs in the background and triggers vacuum on tables when:
- The number of dead tuples exceeds `autovacuum_vacuum_threshold + autovacuum_vacuum_scale_factor * estimated_live_tuples`
- The number of inserted tuples since last vacuum exceeds the insert threshold (for visibility map maintenance)

Default thresholds: vacuum_threshold=50, vacuum_scale_factor=0.2 (vacuum when dead tuples exceed 20% of the table).

---

## Transaction lifecycle

```rust
// Begin
let xid = xid_manager.next_xid();
xid_manager.mark_active(xid);
let snapshot = snapshot_manager.take_snapshot();
let txn = Transaction { xid, snapshot, status: Active };

// Execute operations
// Each operation: write WAL record, modify buffer pool pages

// Commit
wal.write(CommitRecord { xid, timestamp: now() });
wal.flush();  // fdatasync -- durability guarantee
xid_manager.mark_committed(xid);
lock_manager.release_all(xid);
xid_manager.mark_inactive(xid);

// Abort
wal.write(AbortRecord { xid });
// Undo all modifications in reverse order (using WAL prev_lsn chain)
for record in wal.undo_chain(xid).rev() {
    undo(record);
}
xid_manager.mark_aborted(xid);
lock_manager.release_all(xid);
xid_manager.mark_inactive(xid);
```

---

## The two-phase locking relationship

oigrap is not a two-phase locking (2PL) system. 2PL acquires locks on every read and hold them until commit. This prevents all anomalies but causes readers to block writers and vice versa.

MVCC avoids this: readers never block writers, writers never block readers. Only writer-writer conflicts require waiting (two transactions trying to update the same row). This is the fundamental performance advantage of MVCC over 2PL for read-heavy workloads.

The only place 2PL appears in oigrap is in DDL: schema modifications use table-level ACCESS EXCLUSIVE locks, which do block all concurrent operations. This is acceptable — schema changes are rare and expected to be brief.
