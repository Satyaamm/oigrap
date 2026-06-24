# Vector Index

The vector index stores high-dimensional floating-point vectors and supports approximate nearest neighbor (ANN) search: given a query vector, find the K vectors in the index most similar to it by some distance metric.

oigrap implements HNSW (Hierarchical Navigable Small World) from scratch as the primary vector index. HNSW is the state of the art for in-memory ANN search: it achieves recall > 0.99 at query latency < 1ms for million-scale datasets.

---

## The ANN problem

Given:
- A dataset of N vectors, each D-dimensional: V = {v1, v2, ..., vN} where vi ∈ R^D
- A query vector q ∈ R^D
- A distance metric dist(a, b)
- A result count K

Find: the K vectors in V closest to q by dist.

### Exact vs approximate

Exact KNN requires computing dist(q, vi) for all N vectors and sorting. Cost: O(N * D) distance computations. For N=1M, D=1536 (OpenAI embedding size), this is 1.5 billion float operations per query. At 10^9 FLOP/s: ~1.5 seconds per query. Unacceptable.

ANN trades recall (the fraction of true nearest neighbors returned) for speed. At recall=0.99, HNSW returns 99% of the true K nearest neighbors and runs in O(log(N)) distance computations. For N=1M: ~20 distance computations per query instead of 1M.

---

## Distance metrics

```rust
enum DistanceMetric {
    L2,        // Euclidean: sqrt(sum((a[i]-b[i])^2))
    Cosine,    // 1 - dot(a,b) / (|a| * |b|)
    DotProduct, // -dot(a,b)  (negated, so smaller is closer)
    Hamming,   // count of positions where bits differ (for binary vectors)
}
```

L2 is the standard for most embedding models. Cosine is equivalent to L2 when vectors are normalized to unit length. DotProduct is used for some recommendation model embeddings.

In practice, most users normalize their vectors before storage, making L2 and Cosine equivalent. oigrap normalizes on insert if the index is created with cosine distance to ensure correct behavior.

### SIMD-accelerated distance computation

Distance computation is the inner loop of ANN search. It must be maximally fast.

```rust
fn l2_distance(a: &[f32], b: &[f32]) -> f32 {
    // Scalar fallback
    a.iter().zip(b.iter())
     .map(|(x, y)| (x - y) * (x - y))
     .sum::<f32>()
     .sqrt()
}

// AVX2 version: processes 8 f32s per instruction
// For D=1536: 192 AVX2 iterations instead of 1536 scalar iterations
unsafe fn l2_distance_avx2(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = _mm256_setzero_ps();
    let chunks = a.len() / 8;
    for i in 0..chunks {
        let av = _mm256_loadu_ps(a[i*8..].as_ptr());
        let bv = _mm256_loadu_ps(b[i*8..].as_ptr());
        let diff = _mm256_sub_ps(av, bv);
        acc = _mm256_fmadd_ps(diff, diff, acc);  // acc += diff * diff
    }
    // horizontal sum of acc + handle tail
    horizontal_sum_avx2(acc).sqrt()
}
```

---

## HNSW: Hierarchical Navigable Small World

HNSW (Malkov & Yashunin, 2018) builds a multilayer graph where:
- Layer 0 contains all vectors (the densest layer)
- Higher layers contain exponentially fewer vectors (random subset)
- Each node is connected to its M nearest neighbors within its layer
- Search starts at the top (sparsest) layer and descends

### Key parameters

| Parameter | Description | Default |
|-----------|-------------|---------|
| M | Max neighbors per node at non-zero layers | 16 |
| M0 | Max neighbors at layer 0 (usually 2*M) | 32 |
| ef_construction | Search width during index build | 100 |
| ef_search | Search width during query | 64 |
| ml | Level multiplier: 1/ln(M) | 0.36 (for M=16) |

### Index structure

```rust
struct HnswIndex {
    vectors: Vec<Vec<f32>>,              // all vectors, indexed by node_id
    layers: Vec<Vec<Vec<u32>>>,          // layers[node_id][layer] -> [neighbor_ids]
    entry_point: u32,                    // node_id at top layer
    max_layer: usize,
    params: HnswParams,
    metric: DistanceMetric,
}
```

Memory: for N=1M vectors, D=1536, M=16:
- Vector storage: 1M * 1536 * 4 bytes = 6 GB (float32)
- Graph storage: 1M * 16 neighbors/layer * ~4 layers * 4 bytes = 256 MB
- Total: ~6.3 GB for 1M OpenAI-size embeddings

For larger datasets, DiskANN stores graphs and vectors on NVMe SSD (see section below).

### Layer assignment

When inserting a new node, its maximum layer is chosen randomly:

```rust
fn random_layer(ml: f64, rng: &mut Rng) -> usize {
    let r: f64 = rng.gen(); // uniform [0, 1)
    (-r.ln() * ml).floor() as usize
}
```

With ml = 1/ln(M) ≈ 0.36 for M=16:
- P(layer >= 0) = 1.0 (every node is in layer 0)
- P(layer >= 1) ≈ 0.36
- P(layer >= 2) ≈ 0.13
- P(layer >= 3) ≈ 0.05

This geometric distribution gives the index its hierarchical "skyscraper" structure. Most nodes are only in layer 0. A few are in multiple layers as "highway" nodes.

### Insertion algorithm

```rust
fn insert(&mut self, vector: Vec<f32>) -> NodeId {
    let node_id = self.vectors.len() as u32;
    self.vectors.push(vector.clone());
    let node_layer = random_layer(self.params.ml, &mut self.rng);

    // Initialize neighbor lists
    for l in 0..=node_layer {
        self.layers.entry(node_id).or_default().push(Vec::new());
    }

    // Search for nearest neighbors to this new node
    let ep = self.entry_point;
    let mut current_ep = vec![ep];

    // Descend from top layer to node_layer+1 (greedy search only)
    for layer in (node_layer+1..=self.max_layer).rev() {
        current_ep = self.search_layer(&vector, &current_ep, 1, layer);
    }

    // From node_layer down to 0: search and connect
    for layer in (0..=node_layer.min(self.max_layer)).rev() {
        let candidates = self.search_layer(&vector, &current_ep, self.params.ef_construction, layer);

        // Select M best candidates as neighbors (heuristic selection)
        let neighbors = self.select_neighbors(&vector, &candidates, self.params.m(layer));

        // Connect node to its neighbors
        self.layers[node_id as usize][layer] = neighbors.clone();

        // Connect neighbors back to node (bidirectional)
        for &neighbor in &neighbors {
            let neighbor_neighbors = &mut self.layers[neighbor as usize][layer];
            neighbor_neighbors.push(node_id);
            // If neighbor has too many connections, prune to M
            if neighbor_neighbors.len() > self.params.m_max(layer) {
                let pruned = self.select_neighbors(
                    &self.vectors[neighbor as usize],
                    neighbor_neighbors,
                    self.params.m_max(layer)
                );
                self.layers[neighbor as usize][layer] = pruned;
            }
        }

        current_ep = candidates;
    }

    // Update entry point if new node is at a higher layer
    if node_layer > self.max_layer {
        self.entry_point = node_id;
        self.max_layer = node_layer;
    }

    node_id
}
```

### Search algorithm

```rust
fn search(&self, query: &[f32], k: usize, ef: usize) -> Vec<(f32, NodeId)> {
    let mut ep = vec![self.entry_point];

    // Greedy descent from top layer to layer 1
    for layer in (1..=self.max_layer).rev() {
        ep = self.search_layer(query, &ep, 1, layer);
    }

    // Full search at layer 0 with ef candidates
    let candidates = self.search_layer(query, &ep, ef, 0);

    // Return top-k from candidates
    candidates.into_iter()
        .take(k)
        .map(|id| (self.distance(query, &self.vectors[id as usize]), id))
        .collect()
}

fn search_layer(
    &self,
    query: &[f32],
    entry_points: &[NodeId],
    ef: usize,
    layer: usize,
) -> Vec<NodeId> {
    let mut visited: HashSet<NodeId> = entry_points.iter().copied().collect();

    // Min-heap (nearest first): candidates to explore
    let mut candidates: BinaryHeap<(RevOrd<f32>, NodeId)> = entry_points.iter()
        .map(|&ep| (RevOrd(self.distance(query, &self.vectors[ep as usize])), ep))
        .collect();

    // Max-heap (farthest first): current best results
    let mut results: BinaryHeap<(OrdF32, NodeId)> = entry_points.iter()
        .map(|&ep| (OrdF32(self.distance(query, &self.vectors[ep as usize])), ep))
        .collect();

    while let Some((RevOrd(c_dist), c)) = candidates.pop() {
        let worst_result_dist = results.peek().map(|(d, _)| d.0).unwrap_or(f32::MAX);

        // If the nearest candidate is farther than the worst result, stop
        if c_dist > worst_result_dist && results.len() >= ef {
            break;
        }

        // Explore neighbors of c at this layer
        for &neighbor in &self.layers[c as usize][layer] {
            if visited.contains(&neighbor) { continue; }
            visited.insert(neighbor);

            let dist = self.distance(query, &self.vectors[neighbor as usize]);
            let worst = results.peek().map(|(d, _)| d.0).unwrap_or(f32::MAX);

            if dist < worst || results.len() < ef {
                candidates.push((RevOrd(dist), neighbor));
                results.push((OrdF32(dist), neighbor));
                if results.len() > ef {
                    results.pop(); // remove worst
                }
            }
        }
    }

    results.into_sorted_vec().into_iter().map(|(_, id)| id).collect()
}
```

### Neighbor selection heuristic

Simple selection: take the M closest candidates. This works but can produce clusters where many neighbors of a node are close to each other, creating a poor graph structure.

Better: diverse selection (the paper's heuristic). When selecting M neighbors, prefer candidates that are close to the query AND close to previously unselected candidates. This ensures the graph remains well-connected even for clustered data.

```rust
fn select_neighbors_heuristic(
    &self,
    query: &[f32],
    candidates: &[NodeId],
    m: usize,
) -> Vec<NodeId> {
    let mut sorted = candidates.to_vec();
    sorted.sort_by(|a, b| {
        self.distance(query, &self.vectors[*a as usize])
            .partial_cmp(&self.distance(query, &self.vectors[*b as usize]))
            .unwrap()
    });

    let mut result = Vec::new();
    for &c in &sorted {
        if result.len() >= m { break; }
        // Accept c if it is closer to query than to any already-selected neighbor
        let c_dist_to_query = self.distance(query, &self.vectors[c as usize]);
        let is_diverse = result.iter().all(|&r| {
            self.distance(&self.vectors[c as usize], &self.vectors[r as usize]) > c_dist_to_query
        });
        if is_diverse {
            result.push(c);
        }
    }

    // If not enough diverse neighbors, fill with closest remaining
    for &c in &sorted {
        if result.len() >= m { break; }
        if !result.contains(&c) { result.push(c); }
    }

    result
}
```

---

## Integration with query optimizer

The vector index is integrated into the optimizer's cost model as a scan operator. Vector distance queries compile to `VectorScan` physical plan nodes.

```sql
SELECT id, title
FROM documents
ORDER BY embedding <-> '[0.1, 0.4, ...]'
LIMIT 20;
```

Physical plan:
```
Limit(20)
  Sort(embedding <-> query ASC)
    VectorScan(documents.embedding_idx, query_vec, ef=64, limit=20)
      -- returns 64 candidates sorted by distance
      -- optimizer set ef=64 to ensure high recall for LIMIT=20
```

The ef_search parameter is chosen by the optimizer:
- ef_search >= limit (must search at least as many candidates as we return)
- ef_search scales with recall target: for recall=0.99, ef ≈ 2-4x limit
- Higher ef_search for tables with complex predicate post-filters

### Filtered vector search

When there is a WHERE predicate alongside a vector ORDER BY:

```sql
SELECT id FROM documents
WHERE category = 'science'
ORDER BY embedding <-> query_vec
LIMIT 10;
```

Two strategies depending on estimated selectivity of `category = 'science'`:

**High selectivity (few rows match WHERE):** Post-filter. Run ANN search with ef = limit * (1/selectivity). Apply WHERE filter to candidates. Expand search if not enough rows remain after filtering.

**Low selectivity (many rows match WHERE):** Pre-filter. Identify matching TIDs via B+ tree index scan on `category`. Build an allow-list. Run modified HNSW search that only considers nodes in the allow-list. More expensive per query but more accurate.

---

## Persistence

The HNSW graph is persisted to disk in a compact binary format:

```
hnsw_index_file:
  Header (128 bytes):
    magic:      [u8; 4]    "HNSW"
    version:    u32
    num_nodes:  u64
    dimensions: u32
    metric:     u8
    M:          u32
    M0:         u32
    max_layer:  u32
    entry_point: u32

  Vector data section:
    [f32; num_nodes * dimensions]  -- all vectors, node_id * dimensions offset

  Graph section (per layer, per node):
    For each node_id 0..num_nodes:
      For each layer 0..node_layer:
        neighbor_count: u16
        neighbors: [u32; neighbor_count]
```

On startup, the index is loaded into memory. For large indexes that exceed RAM, the DiskANN layout is used instead (vectors and graph stored on NVMe, only a small cache in memory).

---

## DiskANN (scale-out vector index)

For datasets too large for RAM (>10M vectors at typical embedding sizes), DiskANN (Microsoft Research, 2019) stores the graph on NVMe SSD. It uses product quantization (PQ) to compress vectors for the in-memory neighbor selection step, and only reads full vectors from disk for the final re-ranking step.

DiskANN is scheduled for Phase 7 (after HNSW is production-ready). HNSW covers up to approximately 5-10M vectors in RAM; DiskANN covers 100M+.
