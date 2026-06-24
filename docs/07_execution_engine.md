# Execution Engine

The execution engine takes the physical plan produced by the optimizer and executes it to produce results. Execution is the component that actually moves data.

oigrap uses vectorized (batch-oriented) execution. Instead of processing one row at a time, operators exchange columnar batches of many rows. This design is dramatically more CPU-efficient than row-at-a-time execution for analytical workloads and is competitive with row-at-a-time for OLTP workloads.

---

## Execution models: history and choice

### Row-at-a-time (Volcano model, 1994)

The classic model. Each operator implements `next()` which returns one row. The root operator calls `next()` on its child, which calls `next()` on its child, all the way down to the scan. Data flows up one row per call.

```
Sort.next()
  -> Aggregate.next()
       -> Filter.next()
            -> Scan.next()  -- returns one row from disk
```

Problem: one function call per row per operator. For 10M rows and 5 operators, that is 50M function calls. The call overhead, branch mispredictions, and cache misses dominate.

Advantage: simple to implement, low memory overhead.

### Vectorized execution (MonetDB/X100, 2005)

Each operator implements `next_batch()` which returns a batch of N rows (typically 1024-8192) in columnar format. The batch is a struct of arrays: one array per column. Processing a batch of N rows in a tight loop is CPU-friendly: values fit in cache, SIMD instructions can process multiple values per instruction.

```
Sort.next_batch()                    -> 1024 rows
  -> Aggregate.next_batch()          -> 1024 rows
       -> Filter.next_batch()        -> up to 1024 rows
            -> Scan.next_batch()     -> 1024 rows from disk
```

For 10M rows with batch size 1024: ~9766 batch calls instead of 10M row calls. Each batch call processes 1024 rows in a tight SIMD loop.

oigrap uses vectorized execution.

---

## Batch format

A Batch is the unit of data exchanged between operators. It is a columnar representation: each column is stored as a contiguous array of typed values.

```rust
struct Batch {
    num_rows: usize,
    columns: Vec<Column>,
    schema: SchemaRef,
    selection: Option<Vec<u16>>,  // optional selection vector for filtered batches
}

enum Column {
    Bool(Vec<bool>, BitVec),          // values + null bitmap
    Int32(Vec<i32>, BitVec),
    Int64(Vec<i64>, BitVec),
    Float32(Vec<f32>, BitVec),
    Float64(Vec<f64>, BitVec),
    Bytes(Vec<u32>, Vec<u8>, BitVec), // offsets + data buffer + nulls (for variable-length)
    Vector(Vec<f32>, usize),          // flattened f32 array + dimension
}
```

The null bitmap is a compact bit array: bit i is 0 if column[i] is NULL, 1 if non-null. This is more memory-efficient than storing a bool per value and allows SIMD null checks.

For variable-length types (Text, Bytea, JSON): values are stored in a shared data buffer. The offsets array gives the start position of each value in the data buffer. Value i spans bytes `data[offsets[i]..offsets[i+1]]`.

**Selection vector**: instead of materializing a filtered batch into a new allocation, the filter operator sets a selection vector containing the indices of rows that passed the filter. Downstream operators iterate over selected indices only. This avoids allocating filtered batches when many rows pass the filter.

---

## Operator interface

```rust
trait Operator: Send + Sync {
    fn schema(&self) -> &Schema;
    fn next_batch(&mut self, ctx: &ExecContext) -> Result<Option<Batch>>;
    fn close(&mut self);
}

struct ExecContext {
    txn: TransactionRef,
    snapshot: Snapshot,
    work_mem: usize,          // memory budget for this operator
    cancel: CancellationToken,
}
```

`next_batch` returns `Some(batch)` when rows are available, `None` when the operator is exhausted. Operators are pull-based: the root calls into its children.

---

## Operator implementations

### SeqScan

Reads all pages of a heap table sequentially. Applies MVCC visibility (only returns tuples visible to the current snapshot). Returns rows in columnar batch format.

```rust
struct SeqScan {
    table: HeapFile,
    schema: Schema,
    projection: Vec<ColumnId>,
    filter: Option<Expr>,
    cursor: HeapCursor,
    batch_size: usize,
}

impl Operator for SeqScan {
    fn next_batch(&mut self, ctx: &ExecContext) -> Result<Option<Batch>> {
        let mut batch_builder = BatchBuilder::new(&self.schema, self.batch_size);

        while !batch_builder.is_full() {
            match self.cursor.next(ctx.snapshot) {
                None => {
                    return if batch_builder.is_empty() { Ok(None) }
                           else { Ok(Some(batch_builder.build())) };
                }
                Some(tuple) => {
                    if let Some(filter) = &self.filter {
                        if !eval_bool(filter, &tuple) { continue; }
                    }
                    batch_builder.append(&tuple, &self.projection);
                }
            }
        }

        Ok(Some(batch_builder.build()))
    }
}
```

Prefetching: the SeqScan issues read-ahead I/O requests for the next N pages while processing the current page. This hides disk latency for sequential access patterns.

### IndexScan

Uses a B+ tree index to find matching TIDs, then fetches tuples from the heap by TID. For each TID returned by the index, pins the heap page and reads the tuple.

For low-selectivity queries (few matching rows), index scan is faster than sequential scan because it avoids reading irrelevant pages. The crossover point (where seq scan becomes faster) is roughly 5-10% of table rows.

### BitmapScan

Two phases:
1. Scan the index, collecting all matching TIDs into a bitmap (one bit per page-slot pair).
2. Sort the TIDs by page, then read each heap page exactly once, returning all matching tuples.

This converts random I/O (of plain index scan) into sequential I/O when many rows match. Better than index scan for moderate selectivity (5-20%).

### ColumnarScan

Reads columnar segments for the requested columns. Decompresses and decodes each column segment. Applies vectorized filter operations on the decoded batches.

Vectorized filter example (for `age > 30` on INT32 column):
```rust
fn filter_gt_i32(values: &[i32], threshold: i32, nulls: &BitVec) -> Vec<u16> {
    let mut selection = Vec::new();
    for (i, &val) in values.iter().enumerate() {
        if nulls.get(i) && val > threshold {
            selection.push(i as u16);
        }
    }
    selection
}

// SIMD version (AVX2, processes 8 i32s simultaneously):
fn filter_gt_i32_simd(values: &[i32], threshold: i32) -> Vec<u16> {
    let thresh_vec = _mm256_set1_epi32(threshold);
    // ... process 8 values at a time using _mm256_cmpgt_epi32
}
```

### VectorScan

Queries the HNSW index for approximate nearest neighbors of a query vector. Returns batches of (TID, distance) pairs sorted by distance ascending.

The VectorScan operator knows the LIMIT from the query plan and uses it to determine ef_search (the HNSW search parameter controlling recall vs. speed tradeoff).

### HashJoin

Classic hash join in two phases:

**Build phase**: consume all rows from the inner (smaller) child. Insert each row into an in-memory hash table keyed by the join column(s). If the inner exceeds work_mem, spill hash buckets to disk (grace hash join).

**Probe phase**: consume batches from the outer child. For each batch, probe the hash table for matching inner rows. Emit matched pairs.

```rust
struct HashJoin {
    outer: Box<dyn Operator>,
    inner: Box<dyn Operator>,
    build_key: Vec<Expr>,
    probe_key: Vec<Expr>,
    hash_table: Option<HashMap<HashKey, Vec<Batch>>>,
    spill_files: Vec<SpillFile>,
    probe_cursor: ProbeCursor,
}
```

For null handling: NULL != NULL in join conditions by default. Null values do not match in the hash table.

For left outer joins: track which inner rows had no match and emit them with NULL for the outer columns.

### MergeJoin

Requires both inputs sorted on the join key. Advances two sorted cursors simultaneously, emitting matches.

```rust
fn next_batch(&mut self, ctx: &ExecContext) -> Result<Option<Batch>> {
    // Advance both cursors to the point where keys are equal.
    // Handle the "inner group" (multiple inner rows with same key).
    // For each outer row, iterate through matching inner rows.
    // When outer key advances past inner key, advance inner cursor.
}
```

MergeJoin is excellent when both inputs come from index scans on the join key (already sorted) because it avoids the hash table build cost and works in O(n+m) with O(1) memory.

### HashAggregate

Computes GROUP BY aggregations using a hash table.

Build: for each row, hash the group-by keys. Find or create the group's aggregation state in the hash table. Update the state with the new row (running sum, count, min/max, etc.).

After all input is consumed: iterate the hash table and emit one row per group.

```rust
struct AggState {
    count: u64,
    sum: f64,
    min: Value,
    max: Value,
    values: Vec<Value>,  // for array_agg, string_agg
}
```

If the number of groups exceeds work_mem, spill groups to disk and merge. This is the external aggregation algorithm.

### Sort

For data that fits in work_mem: in-memory sort using pdqsort (pattern-defeating quicksort, the algorithm used in Rust's standard library sort).

For data that exceeds work_mem: external merge sort.
1. Read batches of data, sort each batch in memory, write to a run file.
2. After all input: merge all run files simultaneously (k-way merge using a priority queue).

The sort operates on SortKey columns with direction (ASC/DESC) and null ordering (NULLS FIRST/LAST).

### TopK

For `ORDER BY key LIMIT N` without offset: use a fixed-size heap of size N. Process input one batch at a time. For each row, push onto the heap if it would be in the top N. After all input: extract all N elements in sorted order.

Cost: O(rows * log(N)) instead of O(rows * log(rows)) for full sort. Dramatically faster for small N.

### Filter

Apply a predicate expression to each batch. Build a selection vector of indices that pass the predicate. Pass the batch with selection vector to the next operator (no allocation needed if many rows pass).

Expression evaluation is vectorized: evaluate the predicate on entire columns at once using tight loops.

### Project

Evaluate projection expressions on each input batch. Expressions are evaluated column-by-column where possible (e.g., `a + b` is evaluated as a vector addition of column a and column b, not row-by-row).

### Insert / Update / Delete

Modification operators receive rows from their input (which may be a scan + filter + sort), apply the modification to the heap, and update indexes.

Write the WAL record before modifying the page. Pin the page. Write the modification. Unpin (dirty). Return modified TID as output row.

---

## Expression evaluation

Expressions are evaluated in a vectorized manner. The expression evaluator compiles expression trees into a sequence of operations on column arrays.

```rust
enum CompiledExpr {
    Column(usize),                     // direct column reference (zero copy)
    Literal(Value),                    // constant
    BinaryOp(BinaryOp, Box<CompiledExpr>, Box<CompiledExpr>),
    Cast(Box<CompiledExpr>, DataType),
    FunctionCall(BuiltinFn, Vec<CompiledExpr>),
    Case { when_clauses: Vec<(CompiledExpr, CompiledExpr)>, else_expr: Box<CompiledExpr> },
}

fn eval(expr: &CompiledExpr, batch: &Batch) -> Column {
    match expr {
        CompiledExpr::Column(idx) => batch.columns[*idx].clone(),  // zero-copy reference
        CompiledExpr::Literal(v) => Column::constant(v, batch.num_rows),
        CompiledExpr::BinaryOp(op, left, right) => {
            let l = eval(left, batch);
            let r = eval(right, batch);
            eval_binary_op(*op, &l, &r)  // vectorized: processes entire column at once
        }
        // ...
    }
}
```

---

## Execution context and cancellation

Every operator checks `ctx.cancel` at the start of each `next_batch` call. If cancelled (client disconnected or statement timeout), the operator returns an error immediately. All pinned pages are unpinned in the Drop implementation of each operator.

Memory tracking: each operator reports its peak memory usage to a central memory manager. If the query exceeds its total memory budget (`work_mem * operator_count`), operators with the highest memory usage are forced to spill to disk.

---

## Result streaming

Results are not accumulated in memory. The execution engine streams batches from the root operator to the wire layer. The wire layer encodes each batch into PostgreSQL DataRow messages and writes them to the TCP connection. This means queries over very large result sets do not require holding all results in memory simultaneously.

Backpressure: if the TCP send buffer is full (client is reading slowly), the wire layer's write call blocks. This blocks the execution engine's `next_batch` call. Execution naturally slows to match the client's read rate.
