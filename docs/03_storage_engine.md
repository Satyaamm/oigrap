# Storage Engine

The storage engine is the foundation of oigrap. Every component above it depends on correct behavior here. A bug in the storage engine corrupts data. A performance problem here cannot be compensated for anywhere above.

The storage engine manages the relationship between memory and disk. Data lives on disk permanently. The database works on copies in memory. The storage engine controls which pages are in memory, ensures modifications are durable before acknowledging writes, and recovers a consistent state after a crash.

---

## Overview of components

```
+---------------------------+
|     Buffer Pool Manager   |  Memory <-> Disk bridge
+---------------------------+
|     Page Manager          |  Allocate, read, write, free pages
+---------------------------+
|     Heap File Manager     |  Tables as collections of pages
+---------------------------+
|     Free Space Map        |  Track free space per page
+---------------------------+
|     WAL Manager           |  Write-Ahead Log for durability
+---------------------------+
|     Checkpointer          |  Bound recovery time
+---------------------------+
|     Recovery Manager      |  ARIES-based crash recovery
+---------------------------+
```

---

## 1. Pages

The page is the fundamental unit of storage. All I/O happens in page-sized units. A page is 8192 bytes (8KB) by default, configurable at database initialization.

Page size choices and tradeoffs:
- 4KB: matches OS page size exactly, but too small for row data in many cases
- 8KB: PostgreSQL's default, good balance, fits most row data
- 16KB: fewer I/O operations for large rows, worse for small rows
- 32KB: good for columnar segments, too large for OLTP heap files

oigrap uses 8KB for heap files and 64KB blocks for columnar segments.

### Page layout (slotted page)

Every heap page has the same structure:

```
+--------------------------------------------------+  Offset
| Page Header                                      |  0
|   page_id:     u64  (8 bytes)                    |
|   lsn:         u64  (8 bytes) -- WAL LSN         |
|   checksum:    u32  (4 bytes)                    |
|   flags:       u16  (2 bytes)                    |
|   lower:       u16  (2 bytes) -- end of slots    |
|   upper:       u16  (2 bytes) -- start of tuples |
|   special:     u16  (2 bytes) -- special space   |
|   xid_base:    u64  (8 bytes) -- for MVCC pruning|
+--------------------------------------------------+  24
| Slot Array (grows downward from offset 24)        |
|   slot[0]: offset u16, length u16                |
|   slot[1]: offset u16, length u16                |
|   ...                                             |
|   slot[n]: offset u16, length u16                |
+--------------------------------------------------+  lower
|                                                   |
|              FREE SPACE                           |
|                                                   |
+--------------------------------------------------+  upper
| Tuple Data (grows upward from end of page)        |
|   newest tuple at 'upper', older ones further up  |
+--------------------------------------------------+  8192
```

`lower` points to the end of the slot array. `upper` points to the start of the tuple region. Free space is between them: `free_space = upper - lower`.

A slot entry is 4 bytes: 2-byte page offset + 2-byte tuple length. A slot with offset=0 and length=0 means the slot is unused (tuple was deleted).

### Tuple layout

A tuple is a sequence of bytes within a page. It has a header followed by column data:

```
+--------------------------------------------------+
| Tuple Header                                      |
|   xmin:     u64  (8 bytes) -- creating XID       |
|   xmax:     u64  (8 bytes) -- deleting XID (0=live)|
|   cid:      u32  (4 bytes) -- command ID         |
|   infomask: u16  (2 bytes) -- status flags       |
|   natts:    u16  (2 bytes) -- number of columns  |
|   null_bitmap: [u8; ceil(natts/8)]               |
+--------------------------------------------------+
| Column Data                                       |
|   Each column value follows alignment rules       |
|   Variable-length columns: 4-byte length prefix  |
+--------------------------------------------------+
```

`infomask` flags include: HEAP_XMIN_COMMITTED, HEAP_XMIN_INVALID, HEAP_XMAX_COMMITTED, HEAP_XMAX_INVALID, HEAP_HAS_NULL, HEAP_HAS_VARWIDTH, HEAP_IS_HOT_UPDATED.

HOT (Heap Only Tuple) updates are an optimization: when an update does not change any indexed column, the new tuple version is placed on the same page and linked from the old version's TID. This avoids updating indexes.

---

## 2. Buffer Pool Manager

The buffer pool is a region of memory divided into fixed-size frames, each the size of one page. The database works exclusively on pages in the buffer pool. A page must be fetched into a frame before any code can read or write it.

### Data structures

```rust
struct BufferPool {
    frames: Vec<Frame>,          // the actual memory
    page_table: HashMap<PageId, FrameId>,  // page_id -> frame_id
    free_list: VecDeque<FrameId>,          // frames currently unused
    replacer: LRUKReplacer,                // eviction policy
    disk: DiskManager,
    wal: WalManager,
}

struct Frame {
    data: [u8; PAGE_SIZE],       // the page bytes
    page_id: Option<PageId>,
    pin_count: AtomicU32,        // number of active holders
    is_dirty: AtomicBool,        // page has been modified
    latch: RwLock<()>,           // concurrent access control
}
```

### Fetch page algorithm

```
fetch_page(page_id):
  1. Lock page_table
  2. If page_table[page_id] exists:
       frame_id = page_table[page_id]
       pin(frame_id)          -- increment pin_count
       record_access(frame_id) -- update LRU-K history
       return &frames[frame_id]
  3. If free_list is not empty:
       frame_id = free_list.pop_front()
  4. Else:
       frame_id = replacer.evict()  -- find unpinned frame to evict
       if frame_id is None: return Err(BufferFull)
       evicted_page_id = frames[frame_id].page_id
       if frames[frame_id].is_dirty:
           flush_page(frame_id)    -- write to disk before evicting
       page_table.remove(evicted_page_id)
  5. disk.read_page(page_id, &mut frames[frame_id].data)
  6. frames[frame_id].page_id = page_id
  7. frames[frame_id].pin_count = 1
  8. frames[frame_id].is_dirty = false
  9. page_table[page_id] = frame_id
  10. record_access(frame_id)
  11. return &frames[frame_id]
```

### LRU-K replacement policy

Standard LRU evicts the page that was least recently accessed. This is optimal only if pages are accessed uniformly. In databases, some pages (root of B+ tree, frequently-scanned header pages) are accessed very often. LRU would evict them only to immediately reload them.

LRU-K tracks the last K access timestamps per frame. The eviction score is the time since the K-th most recent access (or infinity if fewer than K accesses have occurred). Pages accessed fewer than K times (cold pages) are evicted before hot pages. Among cold pages, the one with the oldest first access is evicted first.

K=2 is the standard choice. This prevents sequential scan pollution: a full table scan accesses every page once, giving all pages K=1, so they are evicted before hot B+ tree pages which have K=2 history.

### Page latches

Every frame has a reader-writer latch (not a lock — latches are short-duration physical-level protection, held for the duration of a single operation, never across a wait). Operations that read a page take the read latch. Operations that modify a page take the write latch. The latch is released before returning to the caller.

This is separate from transaction-level locks, which are held for the duration of a transaction and managed by the lock manager.

---

## 3. Disk Manager

The disk manager abstracts OS file operations. It owns the mapping from PageId to file offset and handles all read/write system calls.

```rust
struct DiskManager {
    db_file: File,                    // main data file
    next_page_id: AtomicU64,
}

impl DiskManager {
    fn read_page(&self, page_id: PageId, buf: &mut [u8; PAGE_SIZE]);
    fn write_page(&self, page_id: PageId, data: &[u8; PAGE_SIZE]);
    fn allocate_page(&self) -> PageId;
    fn deallocate_page(&self, page_id: PageId);
}
```

Page layout on disk: page N lives at byte offset `N * PAGE_SIZE`. The file grows as pages are allocated. Deallocated pages are tracked in a free page list (stored in page 0) and reused before extending the file.

File I/O uses `O_DIRECT` where available to bypass the OS page cache. The database manages its own page cache (the buffer pool); going through the OS page cache doubles the memory overhead and can cause stale reads.

For WAL files, O_DIRECT is not used because WAL writes are always sequential and the OS write-ahead behavior is acceptable. WAL durability is achieved by explicit `fdatasync` calls.

---

## 4. Heap File Manager

A table is stored as a heap file: a collection of pages referenced by the table's catalog entry. Pages within the heap file have no inherent ordering — tuples are placed wherever space is available.

```rust
struct HeapFile {
    table_id: TableId,
    first_page: PageId,
    fsm: FreeSpaceMap,
}

impl HeapFile {
    fn insert_tuple(&mut self, txn: &Transaction, data: &[u8]) -> TupleId;
    fn fetch_tuple(&self, tid: TupleId) -> Option<Tuple>;
    fn update_tuple(&mut self, txn: &Transaction, tid: TupleId, data: &[u8]) -> TupleId;
    fn delete_tuple(&mut self, txn: &Transaction, tid: TupleId);
    fn scan(&self, txn: &Transaction) -> HeapScanner;
}
```

Insert algorithm:
1. Consult FSM to find a page with enough free space for the tuple.
2. Fetch that page from the buffer pool.
3. Write WAL record for the insert.
4. Write the tuple into the page's free space region.
5. Update the slot array.
6. Update `lower` and `upper` in the page header.
7. Update FSM entry for this page.
8. Unpin the page (mark dirty).

### Free Space Map (FSM)

The FSM tracks how much free space is available on each page, enabling fast lookup of pages with enough space for a new tuple.

The FSM is implemented as a tree of uint8 values. Each leaf represents one page. The leaf value is `free_space / 256` (so values 0-255 represent 0-65280 bytes of free space, with 8KB pages this is sufficient resolution). Internal nodes store the maximum value of their subtree. This allows finding a page with at least N free bytes in O(log(pages)) time by descending the tree looking for a subtree maximum >= ceil(N/256).

The FSM is stored in dedicated FSM pages within the table's page space (not a separate file). FSM pages are identified by a flag in the page header.

### Visibility Map

The visibility map tracks which pages contain only tuples that are visible to all current transactions (all-visible pages). Vacuum does not need to scan all-visible pages for dead tuples. Index-only scans can skip heap fetches for tuples on all-visible pages.

Like the FSM, the visibility map is stored in dedicated pages within the table's page space.

---

## 5. Write-Ahead Log (WAL)

The WAL is the durability mechanism. The rule: **a data page may not be written to disk unless the WAL record describing the modification has already been written to disk.**

This rule (called Write-Ahead Logging) guarantees that every committed modification can be recovered after a crash by replaying the WAL.

### WAL record format

```
+----------------------------------+
| LSN: u64 (8 bytes)               |  Log Sequence Number: monotonic position in WAL
| Prev LSN: u64 (8 bytes)          |  Previous LSN for this transaction
| XID: u64 (8 bytes)               |  Transaction that generated this record
| Record Type: u8 (1 byte)         |  INSERT, UPDATE, DELETE, COMMIT, ABORT,
|                                  |  CHECKPOINT, HEAP_NEWPAGE, etc.
| Length: u32 (4 bytes)            |  Total record length including header
| Checksum: u32 (4 bytes)          |  CRC32 of the record
+----------------------------------+
| Record Body (variable)           |  Type-specific payload
+----------------------------------+
```

Record types and bodies:

```
HEAP_INSERT:
  table_id: u64
  page_id:  u64
  slot_id:  u16
  tuple:    [u8; tuple_length]

HEAP_UPDATE:
  table_id:   u64
  old_page:   u64
  old_slot:   u16
  new_page:   u64
  new_slot:   u16
  new_tuple:  [u8; tuple_length]

HEAP_DELETE:
  table_id: u64
  page_id:  u64
  slot_id:  u16

XACT_COMMIT:
  xid:       u64
  timestamp: u64

XACT_ABORT:
  xid: u64

CHECKPOINT:
  redo_lsn:     u64  -- WAL position to start redo from
  next_xid:     u64
  active_xids:  [u64]  -- transactions in flight at checkpoint
```

### WAL write path

Every WAL write goes through the WAL buffer (in-memory ring buffer, configurable size, default 64MB). The WAL buffer absorbs bursts of concurrent writes. The WAL is flushed to disk when:
- A transaction commits (the commit record must be on disk before ACKing the client)
- The WAL buffer is full
- The checkpointer requests a flush
- `wal_sync_interval` milliseconds have elapsed (for group commit optimization)

Group commit: multiple transactions committing in close succession can share a single `fdatasync` call. The first committer triggers the sync; others wait briefly and piggyback on the same sync. This amortizes the expensive `fdatasync` across multiple transactions.

### LSN (Log Sequence Number)

The LSN is a monotonically increasing 64-bit byte offset into the WAL file. Every page's header stores the LSN of the most recent WAL record that modified it. The buffer pool enforces: before writing a dirty page to disk, the WAL must have been flushed up to at least the page's LSN (the WAL-before-data rule).

---

## 6. Recovery Manager (ARIES)

On startup after a crash, the recovery manager replays the WAL to restore a consistent state. oigrap implements ARIES (Algorithm for Recovery and Isolation Exploiting Semantics).

ARIES recovery has three phases:

### Phase 1: Analysis

Scan the WAL forward from the last checkpoint. Build the transaction table (which transactions were active at crash time) and the dirty page table (which pages had been modified but not yet flushed to disk). Determine the redo start point: the minimum recovery LSN across all dirty pages (the earliest point from which we need to redo).

### Phase 2: Redo

Scan the WAL forward from the redo start point. For each WAL record, check if the affected page is in the dirty page table and if the page's on-disk LSN is less than the WAL record's LSN. If so, redo the operation (re-apply the modification to the page). This restores the database to the exact state it was in at the crash moment, including uncommitted changes.

### Phase 3: Undo

Process all transactions that were active at crash time (neither committed nor aborted). Undo their modifications in reverse LSN order (newest first). Each undo operation writes a Compensation Log Record (CLR) to the WAL so that if recovery itself crashes, undo is not repeated.

After undo, only committed transactions' modifications remain in the database. The database is consistent.

### Checkpointing

Checkpoints reduce recovery time. A fuzzy checkpoint (used by oigrap):
1. Write a checkpoint-begin record to WAL.
2. Flush all dirty pages in the buffer pool that have LSN < checkpoint_lsn to disk.
3. Write a checkpoint-complete record to WAL containing: redo_lsn, active transaction table, dirty page table.
4. Update the control file with the checkpoint LSN.

After a successful checkpoint, WAL records before redo_lsn are not needed for recovery and can be archived or deleted.

---

## 7. Putting it together: the durability contract

A transaction T commits at time t. oigrap guarantees:

1. T's WAL records are written to the WAL buffer at the time each operation executes.
2. On COMMIT, a COMMIT WAL record is written and the WAL is flushed to disk (fdatasync).
3. The COMMIT acknowledgment is sent to the client only after the fdatasync returns.
4. At any point after the client receives the COMMIT acknowledgment, a crash and restart will recover all of T's modifications, regardless of whether the modified data pages were written to disk.

This is fsync-based durability. No data is lost as long as the storage device honors fdatasync semantics. On systems with battery-backed write caches, this is the strongest durability guarantee possible.
