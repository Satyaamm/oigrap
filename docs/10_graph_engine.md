# Graph Engine

Graph queries traverse relationships between entities. Social networks (friends of friends), knowledge graphs (entity relationships), dependency trees (package dependencies), lineage graphs (data lineage) — all are fundamentally graph problems.

oigrap does not implement a separate graph storage engine. Instead, graphs are stored in regular relational tables and graph traversal is implemented as optimized recursive query execution. This is the "property graph relational" approach: graphs live in the same storage as all other data, with no schema migration when you want to add graph queries.

---

## Graph representation in relational tables

A property graph has nodes and edges. Both have properties (key-value attributes).

```sql
-- Nodes table
CREATE TABLE nodes (
    id       BIGINT PRIMARY KEY,
    label    TEXT,          -- node type: 'Person', 'Company', 'Product'
    props    JSONB          -- node properties
);

-- Edges table
CREATE TABLE edges (
    id        BIGINT PRIMARY KEY,
    src       BIGINT REFERENCES nodes(id),
    dst       BIGINT REFERENCES nodes(id),
    label     TEXT,         -- edge type: 'FOLLOWS', 'WORKS_AT', 'BOUGHT'
    props     JSONB         -- edge properties
);

-- Indexes for efficient traversal
CREATE INDEX ON edges (src, label);   -- outgoing edges from a node
CREATE INDEX ON edges (dst, label);   -- incoming edges to a node
```

This is a flexible representation. Any graph structure fits into these two tables. Queries that add new edge types or node types are just inserts — no schema migration required.

---

## Graph traversal via Recursive CTE

SQL's WITH RECURSIVE (Common Table Expressions) provides a standard mechanism for graph traversal. oigrap implements and optimizes recursive CTEs.

### Basic traversal

```sql
-- Find all nodes reachable from node 1 (BFS)
WITH RECURSIVE reachable AS (
    -- Base case: start node
    SELECT id, label, 0 AS depth
    FROM nodes
    WHERE id = 1

    UNION ALL

    -- Recursive case: neighbors of already-reached nodes
    SELECT n.id, n.label, r.depth + 1
    FROM reachable r
    JOIN edges e ON e.src = r.id
    JOIN nodes n ON n.id = e.dst
    WHERE r.depth < 5  -- depth limit prevents infinite loops
)
SELECT DISTINCT id, label, depth
FROM reachable
ORDER BY depth;
```

### Shortest path

```sql
-- Find shortest path from node 1 to node 100
WITH RECURSIVE paths AS (
    SELECT
        id,
        ARRAY[id] AS path,
        0 AS distance
    FROM nodes WHERE id = 1

    UNION ALL

    SELECT
        n.id,
        p.path || n.id,
        p.distance + 1
    FROM paths p
    JOIN edges e ON e.src = p.id
    JOIN nodes n ON n.id = e.dst
    WHERE NOT (n.id = ANY(p.path))  -- cycle prevention
      AND p.distance < 10
)
SELECT path, distance
FROM paths
WHERE id = 100
ORDER BY distance
LIMIT 1;
```

### Friends-of-friends (k-hop neighborhood)

```sql
-- Find all users within 2 hops of user 42
WITH RECURSIVE fof AS (
    SELECT dst AS friend_id, 1 AS hop
    FROM edges
    WHERE src = 42 AND label = 'FOLLOWS'

    UNION ALL

    SELECT e.dst, fof.hop + 1
    FROM fof
    JOIN edges e ON e.src = fof.friend_id AND e.label = 'FOLLOWS'
    WHERE fof.hop < 2
)
SELECT DISTINCT friend_id, MIN(hop) AS min_hop
FROM fof
WHERE friend_id != 42
GROUP BY friend_id
ORDER BY min_hop;
```

---

## Recursive CTE execution

The execution engine implements recursive CTE as a specialized operator: **RecursiveScan**.

### Naive execution (wrong)

A naive implementation:
1. Evaluate the base case query. Store result in a working table.
2. Evaluate the recursive case query using the working table as input.
3. Add results to the working table.
4. Repeat until no new rows are added.

This is correct but slow: each iteration is a full join between the recursive table and the edges table.

### Iterative frontier approach (correct, fast)

Maintain two tables: the frontier (nodes added in the most recent iteration) and the visited set (all nodes ever added). Each iteration:
1. Join frontier with edges table to find neighbors.
2. Filter out neighbors already in visited (deduplication).
3. New neighbors become the new frontier.
4. Add new frontier to visited.
5. Stop when frontier is empty.

```rust
struct RecursiveScan {
    base_plan: Box<dyn Operator>,       // the base case query
    recursive_plan: Box<dyn Operator>,  // the recursive case, parameterized on frontier
    frontier: Batch,                    // current frontier
    visited: HashSet<TupleKey>,         // deduplication set
    depth: usize,
    max_depth: usize,
}

impl Operator for RecursiveScan {
    fn next_batch(&mut self, ctx: &ExecContext) -> Result<Option<Batch>> {
        // First call: evaluate base case
        if self.depth == 0 {
            self.frontier = self.base_plan.next_batch(ctx)?.unwrap_or_default();
            self.visited = extract_keys(&self.frontier);
            self.depth = 1;
            return Ok(Some(self.frontier.clone()));
        }

        // Subsequent calls: expand frontier
        if self.depth >= self.max_depth || self.frontier.is_empty() {
            return Ok(None);
        }

        // Inject current frontier into recursive plan as the "working table"
        self.recursive_plan.set_input(&self.frontier);
        let mut new_frontier = collect_all(&mut self.recursive_plan, ctx)?;

        // Deduplicate against visited set
        new_frontier.retain(|row| {
            let key = extract_key(row);
            if self.visited.contains(&key) { false }
            else { self.visited.insert(key); true }
        });

        self.frontier = new_frontier;
        self.depth += 1;

        if self.frontier.is_empty() { Ok(None) }
        else { Ok(Some(self.frontier.clone())) }
    }
}
```

### BFS vs DFS ordering

The recursive CTE specification requires UNION (which deduplicates) or UNION ALL (which does not). With UNION, the engine must deduplicate after each iteration. oigrap's `RecursiveScan` uses the visited set for O(1) deduplication.

The traversal order is BFS (all depth-1 nodes before depth-2 nodes). This is natural for the frontier-based approach. DFS would require a stack-based implementation and is less useful for most graph queries.

---

## Graph-specific optimizations

### Index usage for traversal

The critical optimization: use the B+ tree index on `(edges.src, edges.label)` for outgoing traversal. Without this index, each iteration is a sequential scan of the entire edges table.

With the index, each iteration for node N scans only the edges from N — O(degree(N)) instead of O(|E|).

The query optimizer is aware of graph traversal patterns and ensures the recursive join uses index scans:

```
RecursiveScan:
  Base: IndexScan(nodes, id=1)
  Recursive:
    HashJoin(frontier.id = edges.src)
      edges source: IndexScan(edges, src=?, label='FOLLOWS')  -- uses (src,label) index
      frontier source: WorkingTable
```

### Early termination

For path-finding queries (find path from A to B), the recursive scan terminates as soon as the destination node is found. The executor checks the LIMIT and WHERE conditions at each iteration boundary.

### Cycle detection

oigrap's recursion uses the visited set for implicit cycle detection. A node is not expanded if it has already been visited. This prevents infinite loops in cyclic graphs.

For queries that need to detect cycles explicitly (find all nodes in cycles), a different formulation is needed:

```sql
-- Detect cycles: find nodes that appear in their own reachability set
WITH RECURSIVE reachable(start_id, current_id, path) AS (
    SELECT id, id, ARRAY[id] FROM nodes

    UNION ALL

    SELECT r.start_id, e.dst, r.path || e.dst
    FROM reachable r
    JOIN edges e ON e.src = r.current_id
    WHERE NOT (e.dst = ANY(r.path))
)
SELECT DISTINCT start_id
FROM reachable
WHERE current_id = start_id AND array_length(path, 1) > 1;
```

---

## Adjacency list columnar storage

For large graphs with primarily read-only traversal (knowledge graphs, product recommendation graphs), oigrap supports a columnar adjacency list format that is significantly faster than the normalized edges table.

```sql
-- Columnar adjacency list table
CREATE TABLE adj (
    src    BIGINT,
    dsts   BIGINT[],    -- all destination node IDs for this src
    labels TEXT[]       -- labels for each corresponding edge
) STORAGE COLUMNAR;

-- Sorted by src for efficient lookup
CREATE INDEX ON adj (src);
```

For a query that expands a frontier of 1000 nodes, the columnar adjacency format reads one batch of 1000 rows (each with an array of neighbors) instead of joining the edges table 1000 times. This is significantly faster for wide neighborhoods.

The tradeoff: updating the adjacency list on edge insert/delete requires updating the array column for the source node, which is more expensive than inserting a row in the edges table.

---

## Graph algorithms (built-in)

oigrap provides built-in functions for common graph algorithms. These are implemented as optimized recursive operators rather than general SQL recursion:

```sql
-- Shortest path (Dijkstra)
SELECT oigrap_shortest_path('edges', src_col=>'src', dst_col=>'dst',
                             weight_col=>'distance',
                             start=>1, end=>100);

-- PageRank (iterative, converges after ~20 iterations typically)
SELECT oigrap_pagerank('edges', src_col=>'src', dst_col=>'dst',
                       damping=>0.85, iterations=>20);

-- Weakly connected components
SELECT oigrap_wcc('edges', src_col=>'src', dst_col=>'dst');

-- K-core decomposition
SELECT oigrap_kcore('edges', src_col=>'src', dst_col=>'dst', k=>3);
```

These are Phase 3 features. Initial implementation provides only recursive CTE support.

---

## Mixed graph + vector + relational queries

The real differentiator: graph traversal combined with vector search and relational joins in one query.

```sql
-- Find papers similar to a query embedding, authored by collaborators of researcher #42
-- (2-hop collaboration graph + vector similarity + relational join)

WITH RECURSIVE collaborators AS (
    SELECT dst AS coauthor_id
    FROM edges
    WHERE src = 42 AND label = 'COAUTHORED'

    UNION ALL

    SELECT e.dst
    FROM collaborators c
    JOIN edges e ON e.src = c.coauthor_id AND e.label = 'COAUTHORED'
    WHERE hop < 2
)
SELECT
    p.title,
    p.embedding <-> '[0.2, 0.5, ...]' AS relevance,
    p.citation_count
FROM papers p
WHERE p.author_id IN (SELECT coauthor_id FROM collaborators)
ORDER BY relevance
LIMIT 20;
```

This query: 2-hop graph traversal + ANN vector search + relational IN predicate, all in one SQL statement, planned by one optimizer, executed by one engine. No pipelines. No application-layer orchestration.
