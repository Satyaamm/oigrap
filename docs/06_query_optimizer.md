# Query Optimizer

The query optimizer is the most intellectually complex component in a database system. It takes a logical description of what data to compute (the AST) and produces a physical plan: a concrete sequence of operations that the execution engine will carry out.

The optimizer's job is to find the physical plan with the minimum estimated cost. For a query joining five tables, there are potentially thousands of join orderings and access method combinations. The optimizer must evaluate enough of them to find a good plan without spending more time planning than executing.

oigrap's optimizer is unique because its cost model must simultaneously understand four workload types: relational row access, columnar batch access, vector ANN search, and graph traversal. No existing production optimizer handles all four.

---

## Overview: three phases

```
AST
 |
 v
+---------------------------+
| Phase 1: Logical Planning |  AST -> Logical Plan Tree
+---------------------------+
 |
 v
+---------------------------+
| Phase 2: Rule-Based       |  Always-beneficial algebraic rewrites
|          Rewriting        |  (predicate pushdown, projection pruning, etc.)
+---------------------------+
 |
 v
+---------------------------+
| Phase 3: Cost-Based       |  Enumerate physical plans via DP
|          Optimization     |  Estimate cost using statistics
|                           |  Select minimum-cost plan
+---------------------------+
 |
 v
Physical Plan
```

---

## Phase 1: Logical Planning

Convert the AST into a tree of logical plan nodes. Logical nodes describe what to compute, not how. They are algebraic: LogicalScan, LogicalFilter, LogicalJoin, LogicalProject, LogicalAggregate, LogicalSort, LogicalLimit.

### Semantic analysis (happens here)

Before building the logical plan, validate:
- All referenced tables exist in the catalog
- All referenced columns exist in their tables and are unambiguous
- Type compatibility of expressions (you cannot add TEXT to INT)
- Aggregate functions are not nested
- GROUP BY columns match what SELECT references
- Window functions (if supported) are valid

Resolve all column references to (table_alias, column_index) pairs. After this step, no unresolved names remain.

### Logical plan nodes

```rust
enum LogicalNode {
    Scan {
        table: TableId,
        alias: String,
        projection: Vec<ColumnId>,   // columns needed from this table
    },
    Filter {
        input: Box<LogicalNode>,
        predicate: Expr,
    },
    Join {
        left: Box<LogicalNode>,
        right: Box<LogicalNode>,
        join_type: JoinType,         // Inner, Left, Right, Full, Semi, Anti
        condition: Option<Expr>,
    },
    Project {
        input: Box<LogicalNode>,
        expressions: Vec<(Expr, String)>,  // (expr, output_name)
    },
    Aggregate {
        input: Box<LogicalNode>,
        group_by: Vec<Expr>,
        aggregates: Vec<AggExpr>,
    },
    Sort {
        input: Box<LogicalNode>,
        keys: Vec<SortKey>,
    },
    Limit {
        input: Box<LogicalNode>,
        limit: usize,
        offset: usize,
    },
    Union { left: Box<LogicalNode>, right: Box<LogicalNode>, all: bool },
    Intersect { left: Box<LogicalNode>, right: Box<LogicalNode> },
    Except { left: Box<LogicalNode>, right: Box<LogicalNode> },
    Values(Vec<Vec<Expr>>),
    Insert { table: TableId, input: Box<LogicalNode> },
    Update { table: TableId, input: Box<LogicalNode>, assignments: Vec<(ColumnId, Expr)> },
    Delete { table: TableId, input: Box<LogicalNode> },
}
```

### Building the logical plan for SELECT

```
SELECT s.name, COUNT(o.id)
FROM users u
JOIN orders o ON o.user_id = u.id
WHERE u.plan = 'enterprise'
GROUP BY u.name
HAVING COUNT(o.id) > 5
ORDER BY COUNT(o.id) DESC
LIMIT 10;

Logical plan (before rewrites):
  Limit(10)
    Sort([count(o.id) DESC])
      Filter(count(o.id) > 5)               <- HAVING
        Aggregate(group=[u.name], agg=[COUNT(o.id)])
          Filter(u.plan = 'enterprise')      <- WHERE
            Join(u.id = o.user_id, Inner)
              Scan(users as u)
              Scan(orders as o)
```

---

## Phase 2: Rule-Based Rewriting

Apply algebraic transformations that are always beneficial (no cost estimation needed). Rules are applied repeatedly until no rule fires (fixed-point iteration).

### Rule 1: Predicate pushdown

Move filters as close to their source scan as possible. Filtering early reduces the number of rows flowing through subsequent operators.

```
Before:
  Filter(u.plan = 'enterprise' AND o.amount > 100)
    Join(u.id = o.user_id)
      Scan(users)
      Scan(orders)

After (predicates pushed to scans):
  Join(u.id = o.user_id)
    Filter(u.plan = 'enterprise')
      Scan(users)
    Filter(o.amount > 100)
      Scan(orders)
```

A predicate can be pushed below a join if all columns it references come from one side of the join.

### Rule 2: Projection pruning

Eliminate columns from scans that are never referenced by the query. Reading fewer columns reduces I/O, especially for wide tables.

```
Before:
  Project(u.name)
    Scan(users, all_columns)      -- users has 50 columns

After:
  Project(u.name)
    Scan(users, [name, plan])     -- only read needed columns
```

### Rule 3: Join reordering (initial normalization)

Cross joins (FROM a, b, c without explicit join conditions) are converted to a canonical form. Cartesian products are detected and flagged as expensive.

### Rule 4: Subquery decorrelation

Correlated subqueries that the rewriter missed are converted to joins here with full catalog information available.

### Rule 5: Constant folding and simplification

```
WHERE 1 = 1           -> (removed)
WHERE FALSE           -> short-circuit entire plan to empty result
WHERE id = 5 + 3      -> WHERE id = 8
WHERE NOT (x > 5)     -> WHERE x <= 5
```

### Rule 6: LIMIT pushdown

LIMIT can sometimes be pushed through sorts and joins to reduce data flow:

```
Limit(10)
  Sort(order_date DESC)
    Scan(orders)

-- Sort must produce top-10. Can use a top-K heap instead of full sort.
-- Rewritten as:
TopK(10, order_date DESC)
  Scan(orders)
```

---

## Phase 3: Cost-Based Optimization

The core of the optimizer. For each join, multiple physical implementations are possible. For each scan, multiple access methods are possible. The optimizer enumerates combinations and selects the minimum-cost plan.

### Statistics

Cost estimation requires statistics about the data. Statistics are maintained by ANALYZE and updated periodically.

```rust
struct TableStats {
    row_count: u64,
    page_count: u64,
    avg_row_size: u32,
}

struct ColumnStats {
    null_fraction: f64,       // fraction of NULLs
    ndv: u64,                 // number of distinct values
    most_common_vals: Vec<Value>,
    most_common_freqs: Vec<f64>,
    histogram: Histogram,     // equi-depth histogram with 100 buckets
    correlation: f64,         // physical ordering correlation (-1 to 1)
    avg_width: u32,           // average column value width in bytes
}
```

Selectivity estimation for predicates:
- `col = const`: 1/NDV (if const is not in MCV list), or freq from MCV
- `col < const`: fraction of histogram below const
- `col BETWEEN a AND b`: histogram fraction in [a, b]
- `col IN (a, b, c)`: sum of individual selectivities (capped)
- `col IS NULL`: null_fraction
- Conjunction (AND): product of individual selectivities (independence assumption)
- Disjunction (OR): 1 - (1-s1)(1-s2) using inclusion-exclusion

### Cost model

Cost is measured in abstract units. Two components: I/O cost (page reads) and CPU cost (operations on rows/values). I/O cost dominates for large data. CPU cost dominates for in-memory operations.

```
Page read cost:            1.0  (sequential I/O)
Random page read cost:     4.0  (random I/O, worse than sequential)
Tuple processing cost:     0.01 (per tuple, for filter/project)
Hash probe cost:           0.005 (per tuple, for hash join probe)
Comparison cost:           0.001 (per comparison, for sort/merge)
```

### Scan cost estimates

**Sequential scan:**
```
cost = page_count * seq_page_cost
     + row_count * cpu_tuple_cost
```

**Index scan (B+ tree):**
```
-- Only beneficial when selectivity is low (few rows match)
index_rows = row_count * selectivity
cost = ceil(log(leaf_pages) + index_rows * correlation_adjusted_page_cost)
     + index_rows * cpu_index_tuple_cost
     + index_rows * random_page_cost  -- heap fetches
```

Index scan is worse than sequential scan when selectivity > ~5-10%. At that point, sequential scan reads contiguous pages efficiently; index scan creates random I/O.

**Bitmap scan:**
Between index scan and sequential scan. Collects all TIDs from the index, sorts them by page, then reads heap pages in order. Better than index scan for moderate selectivity, better than sequential scan for low selectivity.

**Columnar scan:**
```
cost = column_count * col_page_count * seq_page_cost  -- only needed columns
     + batch_count * decompression_cost
     + row_count * cpu_tuple_cost * 0.1  -- vectorized processing is faster
```

Columnar scan is faster for queries that access few columns (projection pruning is effective) and aggregate many rows.

**Vector scan (ANN):**
```
-- HNSW search cost
ef_search = max(limit, ef_search_param)  -- ef controls recall/speed tradeoff
cost = O(log(n)) * ef_search * distance_compute_cost * vector_dim
     + postfilter_cost(predicate_selectivity, limit)
```

The optimizer balances recall target (how accurate the ANN must be) against cost. Lower ef_search = faster but less accurate. Higher ef_search = slower but more accurate. The optimizer chooses ef_search based on query requirements.

### Dynamic programming join ordering (Selinger algorithm)

For n tables in a query, there are O(n!) join orderings and O(3^n) subsets of tables to consider. DP avoids recomputation by caching the optimal plan for each subset.

```
For each table T in the query:
  best_plan[{T}] = cheapest scan of T  (seq scan, index scan, or vector scan)

For each subset S of tables with |S| = 2, 3, ..., n:
  For each way to split S into (S1, S2) where S1 ∩ S2 = empty:
    if there is a join predicate connecting S1 and S2:
      For each join algorithm (hash join, merge join, nested loop):
        cost = cost(best_plan[S1]) + cost(best_plan[S2]) + join_cost(algo, S1, S2)
        if cost < best_plan[S].cost:
          best_plan[S] = this plan

Return best_plan[all_tables]
```

The subset space is 2^n. For n=10 tables, this is 1024 subsets — fast. For n=15, it is 32768 — still manageable. For n>15, oigrap switches to a greedy heuristic (join the two cheapest relations with a shared predicate at each step).

### Join algorithm costs

**Hash join:**
```
build_cost  = cost(inner) + inner_rows * hash_probe_cost
probe_cost  = cost(outer) + outer_rows * hash_probe_cost
total       = build_cost + probe_cost

-- Additional cost if inner doesn't fit in memory (spills to disk):
if inner_rows * avg_row_size > work_mem:
    spill_cost = inner_pages * 2 * seq_page_cost  -- write + read
    total += spill_cost
```

Hash join is good when one side fits in memory. Worse for small work_mem.

**Merge join:**
```
-- Both inputs must be sorted on the join key
sort_cost = ...  (if not already sorted from an index)
merge_cost = (outer_rows + inner_rows) * cpu_operator_cost
total = sort_cost(outer) + sort_cost(inner) + merge_cost
```

Merge join is good when inputs are already sorted (index scan on join key) or when sorting is cheap compared to hash join spill cost.

**Nested loop join:**
```
total = cost(outer) + outer_rows * cost(inner per outer row)
```

Nested loop is excellent when the inner is accessed by index and outer has few rows. It is catastrophic for large outer relations (O(n*m) without index). The optimizer only picks nested loop when index access on the inner is available and the outer is small.

### Extended cost model for vector and graph

**Vector predicate pushdown decision:**

When a query has both a WHERE predicate and a vector distance ORDER BY LIMIT, two strategies are possible:

Strategy A — Filter first, then search:
```
Apply WHERE predicate -> filtered_rows
For each filtered row, compute distance to query vector
Sort by distance, take LIMIT
Cost: row_count * filter_cost + filtered_rows * distance_compute_cost
```

Strategy B — ANN search first, then filter:
```
HNSW search returning ef_search candidates
For each candidate, apply WHERE predicate
If enough rows pass filter: done. Else: expand search.
Cost: ANN_cost(ef_search) + ef_search * filter_cost
```

Strategy B wins when selectivity is low (WHERE eliminates few rows). Strategy A wins when selectivity is high (WHERE eliminates most rows). The optimizer chooses based on estimated selectivity.

**Graph traversal cost:**

Recursive CTE cost is estimated using graph statistics: average fan-out per node, estimated depth, estimated total nodes visited.

```
fan_out = avg_edges_per_node
depth_estimate = max(query_depth_limit, log(ndv_of_node_id) / log(fan_out))
nodes_visited = fan_out^0 + fan_out^1 + ... + fan_out^depth
             = (fan_out^(depth+1) - 1) / (fan_out - 1)

cost = nodes_visited * index_scan_cost_per_node
```

The optimizer uses this to decide whether to use a specialized graph traversal operator or fall back to iterative SQL execution.

---

## Physical plan nodes

After cost-based optimization, each logical node is replaced by a physical node with a concrete implementation:

```rust
enum PhysicalNode {
    SeqScan      { table: TableId, filter: Option<Expr>, projection: Vec<ColumnId> },
    IndexScan    { index: IndexId, range: IndexRange, filter: Option<Expr> },
    BitmapScan   { index: IndexId, range: IndexRange },
    ColumnarScan { table: TableId, filter: Option<Expr>, columns: Vec<ColumnId> },
    VectorScan   { index: IndexId, query_vec: Expr, ef_search: usize, limit: usize },
    HashJoin     { outer: Box<PhysicalNode>, inner: Box<PhysicalNode>, condition: Expr },
    MergeJoin    { outer: Box<PhysicalNode>, inner: Box<PhysicalNode>, condition: Expr },
    NestedLoop   { outer: Box<PhysicalNode>, inner: Box<PhysicalNode>, condition: Expr },
    HashAggregate { input: Box<PhysicalNode>, group_by: Vec<Expr>, aggs: Vec<AggExpr> },
    StreamAggregate { input: Box<PhysicalNode>, group_by: Vec<Expr>, aggs: Vec<AggExpr> },
    Sort         { input: Box<PhysicalNode>, keys: Vec<SortKey> },
    TopK         { input: Box<PhysicalNode>, k: usize, keys: Vec<SortKey> },
    Limit        { input: Box<PhysicalNode>, limit: usize, offset: usize },
    Project      { input: Box<PhysicalNode>, exprs: Vec<(Expr, String)> },
    Filter       { input: Box<PhysicalNode>, predicate: Expr },
    Insert       { table: TableId, input: Box<PhysicalNode> },
    Update       { table: TableId, input: Box<PhysicalNode>, assignments: Vec<(ColumnId, Expr)> },
    Delete       { table: TableId, input: Box<PhysicalNode> },
    Values       (Vec<Vec<Expr>>),
    GraphExpand  { start: Box<PhysicalNode>, edge_table: TableId, depth: DepthSpec },
}
```

---

## EXPLAIN output

The optimizer generates human-readable query plan output for EXPLAIN and EXPLAIN ANALYZE:

```
EXPLAIN SELECT u.name, COUNT(o.id)
FROM users u JOIN orders o ON o.user_id = u.id
WHERE u.plan = 'enterprise'
GROUP BY u.name HAVING COUNT(o.id) > 5
ORDER BY COUNT(o.id) DESC LIMIT 10;

HashAggregate  (cost=1240.5..1242.1 rows=10 width=24)
  Filter: (count(o.id) > 5)
  ->  Sort  (cost=1220.3..1228.7 rows=3360 width=20)
        Sort Key: count(o.id) DESC
        ->  HashJoin  (cost=44.2..1050.1 rows=3360 width=20)
              Hash Cond: (o.user_id = u.id)
              ->  SeqScan on orders  (cost=0.0..820.0 rows=50000 width=12)
              ->  Hash  (cost=32.8..32.8 rows=920 width=16)
                    ->  IndexScan on users  (cost=0.4..32.8 rows=920 width=16)
                          Index Cond: (plan = 'enterprise')
```

EXPLAIN ANALYZE adds actual runtime statistics: actual rows, actual time, loops.
