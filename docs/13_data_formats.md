# Data Formats

Precise binary formats for pages, WAL records, and columnar segments. These are the on-disk contracts. Changing them requires a migration path.

---

## 1. Page format (8KB heap page)

All offsets are from the start of the page. All integers are little-endian (except where noted).

```
Offset  Size  Field           Description
------  ----  -----           -----------
0       8     page_id         u64: page identifier (file offset = page_id * 8192)
8       8     lsn             u64: WAL LSN of most recent modification
16      4     checksum        u32: CRC32 of entire page (computed with this field = 0)
20      2     flags           u16: page type and status flags
22      2     lower           u16: byte offset of end of slot array
24      2     upper           u16: byte offset of start of tuple data
26      2     special         u16: byte offset of special space (B+tree use)
28      8     xid_base        u64: base XID for compressed tuple headers
36      4     prune_xid       u32: oldest XID among deleters of dead tuples
40      8     _reserved       padding to 48 bytes

--- Slot array starts at offset 48 ---
Each slot: 4 bytes
  Offset 0  u16: tuple offset within page (0 if slot unused)
  Offset 2  u16: tuple length in bytes (0 if slot unused)

--- Free space: between lower and upper ---

--- Tuple data: from upper to end of page (minus special space) ---
Tuples are written from the end of the page backward.
```

**Flags (bit field):**
- Bit 0: PG_PAGE_HAS_FREE_LINES (some slots are unused)
- Bit 1: PG_PAGE_FULL (no room for new tuples)
- Bit 2: PG_PAGE_ALL_VISIBLE (all tuples visible to all transactions)
- Bits 3-15: reserved

**Tuple format:**

```
Offset  Size    Field         Description
------  ------  -----         -----------
0       8       xmin          u64: creating transaction ID
8       8       xmax          u64: deleting transaction ID (0 if live)
16      4       cid           u32: command ID within transaction
20      2       infomask      u16: tuple status flags
22      2       infomask2     u16: additional flags + column count
24      N       null_bitmap   ceil(natts/8) bytes, bit=1 means non-null
24+N    var     column data   attribute values, type-specific encoding
```

**infomask flags:**
- 0x0001: HEAP_HAS_NULL
- 0x0002: HEAP_HAS_VARWIDTH
- 0x0004: HEAP_HAS_EXTERNAL (TOAST pointer present)
- 0x0100: HEAP_XMIN_COMMITTED
- 0x0200: HEAP_XMIN_INVALID
- 0x0400: HEAP_XMAX_COMMITTED
- 0x0800: HEAP_XMAX_INVALID
- 0x1000: HEAP_XMAX_IS_MULTI (xmax is MultiXactId)
- 0x2000: HEAP_UPDATED (this is an updated tuple)
- 0x4000: HEAP_MOVED_OFF (moved by VACUUM FULL, old location)
- 0x8000: HEAP_MOVED_IN (moved by VACUUM FULL, new location)

---

## 2. WAL record format

The WAL is a sequential file. Records are written contiguously with no gaps.

```
WAL record header (33 bytes):
Offset  Size  Field           Description
------  ----  -----           -----------
0       8     lsn             u64: this record's LSN (byte offset in WAL file)
8       8     prev_lsn        u64: previous WAL record's LSN (for undo chain)
16      8     xid             u64: transaction ID (0 for non-transactional)
24      1     rmgr_id         u8: resource manager (HEAP=0, XACT=1, BTREE=2, etc.)
25      1     record_type     u8: type within resource manager
26      4     length          u32: total record length including header
30      4     crc             u32: CRC32 of header (with crc=0) + body

WAL record body (variable): immediately follows header
```

### HEAP resource manager records

**HEAP_INSERT (rmgr=0, type=0):**
```
4       table_id    u32
8       page_id     u64
16      slot_id     u16
18      tuple_len   u16
20      tuple_data  [u8; tuple_len]
```

**HEAP_UPDATE (rmgr=0, type=1):**
```
4       table_id    u32
8       old_page    u64
16      old_slot    u16
18      new_page    u64
26      new_slot    u16
28      old_xmax    u64  (to restore on abort)
36      new_tuple_len u16
38      new_tuple   [u8; new_tuple_len]
```

**HEAP_DELETE (rmgr=0, type=2):**
```
4       table_id    u32
8       page_id     u64
16      slot_id     u16
18      old_xmax    u64  (to restore on abort)
```

**HEAP_NEWPAGE (rmgr=0, type=3):**
```
4       table_id    u32
8       page_id     u64
16      page_data   [u8; 8192]  (full page image)
```
Used during bulk operations and for FPI (full page images) after checkpoints.

### XACT resource manager records

**XACT_COMMIT (rmgr=1, type=0):**
```
4       timestamp   u64  (microseconds since Unix epoch)
12      nrels       u32  (number of relations modified)
16      rel_ids     [u32; nrels]
```

**XACT_ABORT (rmgr=1, type=1):**
```
4       timestamp   u64
```

**CHECKPOINT (rmgr=1, type=2):**
```
4       redo_lsn    u64  (start point for redo after this checkpoint)
12      next_xid    u64  (next XID to assign after recovery)
20      oldest_xid  u64  (oldest active XID)
28      nactive     u32  (number of active transactions)
32      active_xids [u64; nactive]
32+N    ndirty      u32  (number of dirty pages at checkpoint)
32+N+4  dirty_pages [(u32 table_id, u64 page_id, u64 lsn); ndirty]
```

---

## 3. Columnar segment format

A column segment stores up to 65536 values for one column of one row group.

```
Segment header (64 bytes):
Offset  Size  Field             Description
------  ----  -----             -----------
0       8     segment_id        u64: unique segment identifier
8       4     row_count         u32: number of values in this segment
12      1     encoding          u8: 0=Plain, 1=RLE, 2=Delta, 3=Dictionary, 4=BitPack
13      1     compression       u8: 0=None, 1=LZ4, 2=Zstd
14      4     null_count        u32: number of NULL values
18      1     data_type         u8: column data type code
19      4     null_bitmap_size  u32: bytes in null bitmap
23      4     data_size         u32: bytes of column data (after compression)
27      4     uncompressed_size u32: bytes before compression
31      8     min_value         bytes: type-specific minimum (for zone map)
39      8     max_value         bytes: type-specific maximum (for zone map)
47      4     bloom_filter_size u32: bytes in bloom filter (0 if no bloom filter)
51      13    _reserved         padding to 64 bytes

Null bitmap: ceil(row_count / 8) bytes
  bit[i] = 1 -> value i is non-null
  bit[i] = 0 -> value i is null

Bloom filter: bloom_filter_size bytes (if present)
  Standard bloom filter for equality predicate pushdown

Column data: data_size bytes (compressed)
  After decompression: uncompressed_size bytes in encoding-specific format
```

### Plain encoding layout

Values stored as-is in native binary format, tightly packed.

- BOOL: 1 byte per value (0 or 1)
- INT32: 4 bytes per value, little-endian
- INT64: 8 bytes per value, little-endian
- FLOAT32: 4 bytes per value, IEEE 754 little-endian
- FLOAT64: 8 bytes per value, IEEE 754 little-endian
- TEXT/BYTES: 4-byte offset array (u32[row_count+1]) followed by data bytes
  - value[i] spans bytes data[offsets[i]..offsets[i+1]]
  - offsets[0] = 0, offsets[row_count] = total data length

### RLE encoding layout

```
[run_count: u32]
[runs: (value: type, length: u32) * run_count]
```

Value is stored in the native type format. Length is a u32 run length.

### Delta encoding layout

```
[base_value: i64]    first value (stored as signed 64-bit to handle all int types)
[delta_count: u32]
[deltas: zigzag_varint * delta_count]
```

Zigzag varint encoding: signed integers are mapped to unsigned before varint encoding.
- n >= 0: 2*n
- n < 0: 2*abs(n) - 1

Then varint encoded: 7 bits per byte, MSB indicates continuation.

### Dictionary encoding layout

```
[dict_size: u32]
[dict: value * dict_size]    values in native type format, sorted
[codes: varint * row_count]  each code is an index into dict
```

Dictionary values are sorted. Codes are stored as varints (small codes use fewer bytes).

### Bit packing (Frame of Reference) layout

```
[frame: i64]           the minimum value (subtracted from all values)
[bits_per_value: u8]   how many bits each residual requires
[packed_data: bytes]   residuals packed at bits_per_value bits each
```

Residuals are unsigned integers (value - frame). They are packed into the data array without byte alignment: residual[0] occupies bits 0..(bits_per_value-1), residual[1] occupies bits bits_per_value..(2*bits_per_value-1), etc.

---

## 4. B+ tree page format

B+ tree nodes are stored in regular 8KB pages with a special space at the end (referenced by the `special` field in the page header).

**Internal (non-leaf) node layout:**
```
Page header (standard)
Slot array (key offsets)
Key data: key values, one per slot
Special space (B+ tree metadata):
  btree_flags: u16   (is_root, is_leaf, is_rightmost)
  level:       u16   (0 = leaf, increases toward root)
  prev_page:   u64   (left sibling, for leaf-level linked list)
  next_page:   u64   (right sibling)
```

Internal node entries: (key, child_page_id). For N keys, there are N+1 child pointers. The leftmost child holds values < key[0], the second child holds values in [key[0], key[1]), etc.

**Leaf node layout:**
Leaf node entries: (key, TID). Key values are stored in the slot array as normal tuples. TIDs are stored in the tuple data (8-byte page_id + 2-byte slot_id).

For non-unique indexes: multiple TIDs may exist for the same key. These are stored as separate leaf entries.

---

## 5. HNSW index file format

```
Header (256 bytes):
  magic:           [u8; 4]   "HNSW"
  version:         u32       format version
  num_nodes:       u64       total vector count
  dimensions:      u32       vector dimensionality
  metric:          u8        0=L2, 1=Cosine, 2=DotProduct
  M:               u32       max neighbors per node (non-zero layers)
  M0:              u32       max neighbors per node (layer 0)
  ef_construction: u32       construction ef parameter
  max_layer:       u32       current maximum layer
  entry_point:     u64       node ID of entry point
  _reserved:       [u8; 200] padding to 256 bytes

Vector data (immediately after header):
  [f32; num_nodes * dimensions]
  Node i's vector: data[i*dimensions..(i+1)*dimensions]

Layer count array:
  [u8; num_nodes]  -- max_layer for each node

Graph adjacency (variable length):
  For each node_id 0..num_nodes:
    For each layer 0..node_layer[node_id]:
      neighbor_count: u16
      neighbors:      [u32; neighbor_count]
```

The graph adjacency section is read sequentially. To seek to node N's adjacency data, the file also stores an index:

```
Graph offset index (immediately after vector data):
  [u64; num_nodes]  -- byte offset into graph adjacency section for each node
```

---

## 6. Database catalog format

The catalog (system tables) describes the database schema: tables, columns, indexes, types, users. The catalog is stored in the database's own page files under reserved table IDs.

| Table ID | Contents |
|----------|----------|
| 1 | pg_database: all databases |
| 2 | pg_namespace: schemas (namespaces) |
| 3 | pg_class: tables, indexes, sequences |
| 4 | pg_attribute: columns of each table |
| 5 | pg_type: data types |
| 6 | pg_index: index metadata |
| 7 | pg_constraint: constraints (PK, FK, UNIQUE, CHECK) |
| 8 | pg_authid: users and roles |
| 9 | pg_hba: host-based authentication rules |
| 10 | pg_statistic: table and column statistics |

These tables follow the same slotted page format as all other tables. The catalog is bootstrapped at database creation with a fixed layout known to the code.

---

## 7. Control file

The control file (`oigrap.ctrl`) stores critical database state that must be available before the main data files are opened:

```
magic:              [u8; 8]   "OIGRAP\0\0"
version:            u32
pg_control_version: u32       (for PostgreSQL compatibility checks)
catalog_version:    u32       (schema format version)
system_identifier:  u64       (random ID assigned at initdb)
state:              u8        0=starting, 1=running, 2=shutdown, 3=recovery
checkpoint_lsn:     u64       (LSN of latest checkpoint)
prev_checkpoint_lsn: u64      (LSN of second-to-last checkpoint)
checkpoint_copy:    [u8; 256] (inline copy of latest checkpoint WAL record)
time:               u64       (timestamp of last shutdown, microseconds)
next_xid:           u64       (next transaction ID to assign)
oldest_xid:         u64       (oldest XID still needed)
next_oid:           u64       (next object ID to assign)
crc:                u32       (CRC32 of all above fields)
```

The control file is written atomically (write to temp file, fsync, rename). It is always consistent even if the database crashes during a write.
