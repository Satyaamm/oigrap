# Storage Layouts

oigrap stores data in three physical layouts: row store, columnar store, and document store. The same SQL table can use any layout. The query optimizer chooses which physical representation to access based on query shape.

A fourth "dual" mode stores data in both row and columnar format simultaneously and is used for tables with mixed OLTP/OLAP access patterns.

---

## 1. Row Store (N-ary Storage Model, NSM)

The default layout. Suitable for OLTP: frequent point lookups, single-row updates, and transactions that touch a small number of rows.

Row store is what PostgreSQL, MySQL, and SQLite use. All columns of a row are stored contiguously on a slotted heap page. Fetching one row requires one page read (or a few if the row spans pages due to TOAST-style overflow).

### When to use row store

- Tables with frequent point lookups by primary key
- Tables with high write rates (INSERT, UPDATE, DELETE)
- Tables that are accessed in small transactions
- Tables where queries typically access most columns of a row

### Physical organization

Already described in full in `03_storage_engine.md`. Key properties:
- 8KB slotted pages
- TupleID = (page_id, slot_id)
- MVCC tuple headers (xmin, xmax)
- B+ tree indexes for secondary access
- FSM and visibility map maintained per table

### Row store limitations

For analytical queries that read one column across millions of rows (e.g., `SELECT AVG(age) FROM users`), the row store reads entire pages even though only the `age` column is needed. This wastes I/O bandwidth. The columnar store eliminates this waste.

---

## 2. Columnar Store (Decomposition Storage Model, DSM)

Designed for analytical (OLAP) queries: aggregations, scans over large ranges, GROUP BY, and queries that access few columns of wide tables.

In the columnar store, each column of a table is stored in its own file of column segments. A column segment is a block of up to 65536 values for one column. Values within a segment are tightly packed, typed, and compressed.

### When to use columnar store

- Tables that are rarely updated (append-only or bulk-load pattern)
- Tables with wide schemas (many columns) but queries that only access a few
- Aggregation-heavy queries (SUM, AVG, MIN, MAX, COUNT by group)
- Time-series data, event logs, analytics tables

### Physical organization

```
Table: events (id, timestamp, user_id, event_type, properties, amount)
6 columns.

File layout:
  events.id.col          -- all id values, column segment format
  events.timestamp.col   -- all timestamp values
  events.user_id.col     -- all user_id values
  events.event_type.col  -- all event_type values
  events.properties.col  -- all properties values (JSON column)
  events.amount.col      -- all amount values
  events.meta            -- metadata: segment offsets, row counts, min/max per segment
```

Column segment format (per column file):
```
+-----------------------------------------------+
| Segment Header (64 bytes)                      |
|   segment_id:   u64                            |
|   row_count:    u32    (up to 65536)           |
|   encoding:     u8     (Plain, RLE, Delta, Dict)|
|   compression:  u8     (None, LZ4, Zstd)      |
|   null_count:   u32                            |
|   min_value:    [u8; 16]  (type-dependent)     |
|   max_value:    [u8; 16]                       |
|   data_offset:  u32    (offset to data within segment)|
|   null_offset:  u32    (offset to null bitmap) |
|   data_size:    u32    (compressed bytes)      |
+-----------------------------------------------+
| Null Bitmap (ceil(row_count/8) bytes)          |
+-----------------------------------------------+
| Column Data (compressed, encoded)              |
+-----------------------------------------------+
```

### Encoding schemes

Encoding reduces data size before compression. Different encodings suit different data distributions.

**Plain encoding**: values stored as-is in their native binary format. Used for random data with high NDV.

**Run-Length Encoding (RLE)**: for columns with many consecutive repeated values.
```
Input:   [1, 1, 1, 2, 2, 3, 3, 3, 3, 1]
Encoded: [(1, 3), (2, 2), (3, 4), (1, 1)]  -- (value, run_length) pairs
```
Best for: boolean columns, categorical columns with few distinct values (country, status).

**Delta encoding**: for sorted numeric columns, store the difference between consecutive values instead of the values themselves. Deltas are typically small and compress well.
```
Input:   [100, 103, 107, 112, 116]
Deltas:  [100, 3, 4, 5, 4]
```
Best for: timestamps (monotonically increasing), auto-increment IDs, ordered numeric ranges.

**Dictionary encoding**: replace values with small integer codes. Store a dictionary of (code -> value) separately. Codes are compact integers (typically 8-bit or 16-bit for up to 256/65536 distinct values).
```
Input:   ['New York', 'London', 'New York', 'Paris', 'London']
Dict:    {0: 'New York', 1: 'London', 2: 'Paris'}
Codes:   [0, 1, 0, 2, 1]
```
Best for: low-cardinality string columns (city, country, status, category).

**Bit packing (Frame of Reference, FOR)**: for integers in a narrow range, store only the bits needed. If all values in a segment are in [1000, 1020], they span 5 bits (range=20 < 32). Subtract the minimum (frame), store only the 5-bit residuals.
```
Input:   [1003, 1007, 1001, 1019, 1000]
Frame:   1000
Residuals: [3, 7, 1, 19, 0]  -- fits in 5 bits each
```
Best for: numeric columns with moderate range within a segment.

### Segment statistics for pruning

Each column segment stores min and max values in its header. When a WHERE predicate can be evaluated against segment min/max, entire segments can be skipped without decompression.

Example: `WHERE amount > 1000` with segment [min=50, max=800] — skip segment. With segment [min=700, max=1500] — read segment (partial overlap). With segment [min=1200, max=5000] — all rows pass, no filtering needed after decompression.

This is called **segment pruning** or **zone maps** (Snowflake's terminology). For sorted columns, segment pruning eliminates the large majority of I/O for range queries.

### Row group structure

Segments across all columns that cover the same row range are grouped into a **row group**. Row group size is typically 65536 rows (128KB of data for INT64 columns, more after compression).

```
Row group 0: rows 0-65535
  id.col segment 0 (rows 0-65535)
  timestamp.col segment 0
  user_id.col segment 0
  amount.col segment 0
  ...

Row group 1: rows 65536-131071
  id.col segment 1
  timestamp.col segment 1
  ...
```

Row groups are the unit of columnar I/O. A query that reads columns A and B reads segments A[0] and B[0] for row group 0, etc. Columns not in the query are not read.

### Columnar index (bloom filter per segment)

For equality predicates on high-cardinality columns, a per-segment bloom filter allows skipping segments that definitely don't contain the value:

```
WHERE user_id = 42
  Bloom filter for segment 0 says: value 42 not present -> skip
  Bloom filter for segment 1 says: value 42 possibly present -> read
  Bloom filter for segment 2 says: value 42 not present -> skip
```

Bloom filters have a false positive rate but zero false negatives. A "not present" answer is always correct. A "possibly present" answer may require reading the segment and finding no match.

---

## 3. Document Store

The document layout stores JSON documents (JSONB format) alongside optional typed columns. It is designed for flexible-schema data where different rows have different keys.

oigrap does not implement a separate document database engine. Instead, the document layout is a specialization of the row store where one or more columns are of type JSONB.

### JSONB format

JSONB stores JSON in a binary format that allows O(1) key lookup without parsing. The binary format stores:
- A jump table at the start of each object: sorted list of (key_hash, offset) pairs
- Key strings and values stored inline
- Nested objects and arrays recursively encoded

```
JSONB binary layout for {"name": "Alice", "age": 30, "tags": ["vip", "beta"]}:

[type: object]
[entry_count: 3]
[key "age" hash, value offset]    -- sorted by key for binary search
[key "name" hash, value offset]
[key "tags" hash, value offset]
[key_data: "age\0name\0tags\0"]
[value: int32 30]
[value: text "Alice"]
[value: array]
  [entry_count: 2]
  [text "vip"]
  [text "beta"]
```

Key lookup: hash the key, binary search the jump table, follow the offset. O(1) for top-level keys.

### JSON operators

oigrap implements PostgreSQL-compatible JSON operators from scratch:

| Operator | Description | Example |
|----------|-------------|---------|
| `->` int | Get array element by index | `'[1,2,3]'::jsonb -> 1` returns `2` |
| `->` text | Get object field as JSONB | `data -> 'name'` returns `"Alice"` |
| `->>` int | Get array element as text | `data ->> 0` returns `'Alice'` |
| `->>` text | Get object field as text | `data ->> 'name'` returns `Alice` |
| `#>` path | Get field at path as JSONB | `data #> '{address,city}'` |
| `#>>` path | Get field at path as text | `data #>> '{address,city}'` |
| `@>` | Contains | `data @> '{"role":"admin"}'` |
| `<@` | Contained by | `'{"a":1}' <@ data` |
| `?` | Has key | `data ? 'name'` |
| `?|` | Has any key | `data ?| array['name','email']` |
| `?&` | Has all keys | `data ?& array['name','age']` |
| `||` | Concatenate | `data || '{"extra":1}'` |
| `-` | Delete key | `data - 'temp_field'` |

### GIN index on JSONB

A Generalized Inverted Index (GIN) on a JSONB column allows fast lookup of documents containing specific key-value pairs:

```sql
CREATE INDEX ON events USING GIN (data);

-- Query that uses GIN index:
SELECT * FROM events WHERE data @> '{"event_type": "purchase"}';
```

The GIN index stores (key, value_hash) -> [row_ids] entries. An `@>` query looks up all entries matching the query JSON and intersects their row ID lists.

---

## 4. Dual Format (Row + Columnar)

For tables with mixed workloads (both point lookups and analytical scans), oigrap can maintain data in both row and columnar format simultaneously.

The delta store / main store pattern:
- Recent writes go to the row store (fast random writes)
- A background process periodically compresses row store data into columnar segments (the "compaction" process)
- Reads consult both the row store delta (for recent data) and the columnar main store (for older data)
- The query optimizer is aware of both stores and routes scan operations accordingly

This is the same pattern used by SingleStore (MemSQL) and is called the "Row Store / Column Store" hybrid. It is scheduled for Phase 2 of oigrap's development.

---

## Layout selection

Tables default to row store. Users can specify layout at creation time:

```sql
CREATE TABLE events (...) STORAGE COLUMNAR;
CREATE TABLE users (...) STORAGE ROW;          -- default
CREATE TABLE products (...) STORAGE DUAL;      -- both, automatic compaction
```

The `STORAGE AUTO` option (planned, not in initial implementation) profiles access patterns over time and automatically migrates cold data to columnar format.
