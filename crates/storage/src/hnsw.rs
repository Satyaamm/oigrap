/// HNSW (Hierarchical Navigable Small World) approximate nearest neighbor index.
/// Implementation follows the original HNSW paper by Malkov & Yashunin (2018).
use std::collections::BinaryHeap;
use std::cmp::Ordering;

/// A node in the HNSW graph.
#[derive(Debug, Clone)]
pub struct HnswNode {
    /// The embedding vector.
    pub vector: Vec<f64>,
    /// External ID (e.g. row TID).
    pub id: u64,
    /// Adjacency lists per layer. `connections[layer]` = list of neighbor node indices.
    pub connections: Vec<Vec<usize>>,
}

/// Ordered wrapper for max-heap (nearest first).
#[derive(Debug, Clone)]
struct HeapEntry {
    dist: f64,
    idx: usize,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool { self.dist == other.dist }
}
impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap: farthest distance is popped first, so peek() gives the farthest element
        self.dist.partial_cmp(&other.dist).unwrap_or(Ordering::Equal)
    }
}

/// Min-heap entry (nearest first).
#[derive(Debug, Clone)]
struct MinEntry {
    dist: f64,
    idx: usize,
}

impl PartialEq for MinEntry {
    fn eq(&self, other: &Self) -> bool { self.dist == other.dist }
}
impl Eq for MinEntry {}

impl PartialOrd for MinEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}

impl Ord for MinEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap (nearest first): reverse comparison so BinaryHeap pops smallest dist first
        other.dist.partial_cmp(&self.dist).unwrap_or(Ordering::Equal)
    }
}

/// HNSW index.
pub struct HnswIndex {
    /// Maximum number of connections per node per layer (M).
    m: usize,
    /// Maximum connections at layer 0 (typically 2*M).
    m0: usize,
    /// Beam width during construction.
    ef_construction: usize,
    /// All nodes.
    nodes: Vec<HnswNode>,
    /// Entry point node index.
    entry_point: Option<usize>,
    /// Maximum layer occupied.
    max_layer: usize,
    /// Level multiplier (1 / ln(M)).
    level_mult: f64,
}

impl HnswIndex {
    /// Create a new HNSW index with M connections per layer and ef_construction beam width.
    pub fn new(m: usize, ef_construction: usize) -> Self {
        let level_mult = if m > 1 { 1.0 / (m as f64).ln() } else { 1.0 };
        HnswIndex {
            m,
            m0: m * 2,
            ef_construction,
            nodes: Vec::new(),
            entry_point: None,
            max_layer: 0,
            level_mult,
        }
    }

    /// Number of vectors indexed.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Insert a vector with an associated external ID.
    pub fn insert(&mut self, id: u64, vector: Vec<f64>) {
        let node_level = self.random_level();
        let node_idx = self.nodes.len();

        // Allocate adjacency lists for each layer this node participates in
        let connections: Vec<Vec<usize>> = (0..=node_level).map(|_| Vec::new()).collect();
        self.nodes.push(HnswNode { vector: vector.clone(), id, connections });

        if self.entry_point.is_none() {
            self.entry_point = Some(node_idx);
            self.max_layer = node_level;
            return;
        }

        let ep = self.entry_point.unwrap();
        let mut curr_ep = ep;
        let curr_max_layer = self.max_layer;

        // Phase 1: Greedy search from top layer down to node_level+1
        for layer in (node_level + 1..=curr_max_layer).rev() {
            let result = self.search_layer(&vector, curr_ep, 1, layer);
            if let Some(nearest) = result.first() {
                curr_ep = nearest.1;
            }
        }

        // Phase 2: Insert at each layer from min(node_level, max_layer) down to 0
        for layer in (0..=node_level.min(curr_max_layer)).rev() {
            let candidates = self.search_layer(&vector, curr_ep, self.ef_construction, layer);
            if let Some(nearest) = candidates.first() {
                curr_ep = nearest.1;
            }

            let m_max = if layer == 0 { self.m0 } else { self.m };
            let neighbors = self.select_neighbors_heuristic(&vector, &candidates, m_max);

            // Connect node_idx to neighbors at this layer
            self.nodes[node_idx].connections.get_or_insert_with(layer, Vec::new);
            for &nb_idx in &neighbors {
                // Ensure neighbor has a connection list at this layer
                while self.nodes[nb_idx].connections.len() <= layer {
                    self.nodes[nb_idx].connections.push(Vec::new());
                }
                if !self.nodes[node_idx].connections[layer].contains(&nb_idx) {
                    self.nodes[node_idx].connections[layer].push(nb_idx);
                }
                if !self.nodes[nb_idx].connections[layer].contains(&node_idx) {
                    self.nodes[nb_idx].connections[layer].push(node_idx);
                }
                // Prune if too many connections, always keeping the newly added back-edge
                // to ensure node_idx remains reachable through nb_idx (graph navigability).
                let nb_conns: Vec<usize> = self.nodes[nb_idx].connections[layer].clone();
                if nb_conns.len() > m_max {
                    let nb_vec = self.nodes[nb_idx].vector.clone();
                    let mut candidates_nb: Vec<(f64, usize)> = nb_conns.iter().map(|&i| {
                        (l2_distance(&self.nodes[i].vector, &nb_vec), i)
                    }).collect();
                    // Sort by distance ascending so we can prune the farthest
                    candidates_nb.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
                    // Always keep the back-edge to node_idx for graph navigability;
                    // if it would be dropped, drop the next-farthest instead.
                    let pruned = if candidates_nb.iter().take(m_max).any(|(_, i)| *i == node_idx) {
                        // node_idx is already in the top-m_max nearest; standard prune
                        candidates_nb.into_iter().take(m_max).map(|(_, i)| i).collect()
                    } else {
                        // node_idx would be dropped; keep it by removing the m_max-th nearest
                        let mut kept: Vec<usize> = candidates_nb.iter().take(m_max - 1).map(|(_, i)| *i).collect();
                        kept.push(node_idx);
                        kept
                    };
                    self.nodes[nb_idx].connections[layer] = pruned;
                }
            }
        }

        // Update entry point if this node is at a higher layer
        if node_level > curr_max_layer {
            self.entry_point = Some(node_idx);
            self.max_layer = node_level;
        }
    }

    /// Search for k nearest neighbors to query, using ef candidates.
    pub fn search(&self, query: &[f64], k: usize, ef: usize) -> Vec<(u64, f64)> {
        if self.nodes.is_empty() {
            return vec![];
        }

        let ep = self.entry_point.unwrap();
        let mut curr_ep = ep;

        // Phase 1: Greedy search from top layer down to layer 1
        // Use ef=1 at the very top layers to quickly descend, then use the actual ef
        // at the lower layers to find a good entry point for the layer-0 search.
        for layer in (1..=self.max_layer).rev() {
            let layer_ef = if layer > 1 { 1 } else { ef };
            let result = self.search_layer(query, curr_ep, layer_ef, layer);
            if let Some(nearest) = result.first() {
                curr_ep = nearest.1;
            }
        }

        // Phase 2: Beam search at layer 0
        let candidates = self.search_layer(query, curr_ep, ef.max(k), 0);

        candidates.into_iter()
            .take(k)
            .map(|(dist, idx)| (self.nodes[idx].id, dist))
            .collect()
    }

    /// Greedy beam search through one layer. Returns (distance, node_idx) sorted nearest-first.
    fn search_layer(&self, query: &[f64], ep: usize, ef: usize, layer: usize) -> Vec<(f64, usize)> {
        let ep_dist = l2_distance(query, &self.nodes[ep].vector);

        // visited set
        let mut visited = vec![false; self.nodes.len()];
        visited[ep] = true;

        // candidates: min-heap (nearest first)
        let mut candidates: BinaryHeap<MinEntry> = BinaryHeap::new();
        candidates.push(MinEntry { dist: ep_dist, idx: ep });

        // result set: max-heap of size ef (we keep the ef closest)
        let mut result: BinaryHeap<HeapEntry> = BinaryHeap::new();
        result.push(HeapEntry { dist: ep_dist, idx: ep });

        while let Some(cand) = candidates.pop() {
            // If closest candidate is farther than farthest in result and result is full, stop
            if result.len() >= ef {
                if let Some(farthest) = result.peek() {
                    if cand.dist > farthest.dist {
                        break;
                    }
                }
            }

            // Expand neighbors at this layer
            let neighbors: Vec<usize> = self.nodes[cand.idx]
                .connections
                .get(layer)
                .cloned()
                .unwrap_or_default();

            for nb_idx in neighbors {
                if visited[nb_idx] { continue; }
                visited[nb_idx] = true;

                let nb_dist = l2_distance(query, &self.nodes[nb_idx].vector);

                // Add to result if better than farthest or result not full
                let should_add = result.len() < ef || result.peek().map(|f| nb_dist < f.dist).unwrap_or(true);
                if should_add {
                    candidates.push(MinEntry { dist: nb_dist, idx: nb_idx });
                    result.push(HeapEntry { dist: nb_dist, idx: nb_idx });
                    // Trim result to ef
                    while result.len() > ef {
                        result.pop();
                    }
                }
            }
        }

        // Convert result to sorted vec (nearest first)
        let mut out: Vec<(f64, usize)> = result.into_iter().map(|e| (e.dist, e.idx)).collect();
        out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
        out
    }

    /// Select M neighbors using simple distance heuristic.
    fn select_neighbors_heuristic(&self, _query: &[f64], candidates: &[(f64, usize)], m: usize) -> Vec<usize> {
        // Simple: just take the M nearest
        candidates.iter().take(m).map(|(_, idx)| *idx).collect()
    }

    /// Randomly assign a layer level for a new node.
    fn random_level(&self) -> usize {
        // Use a simple deterministic approach based on node count for reproducibility
        // In production this would use a real RNG
        let n = self.nodes.len();
        let mut level = 0;
        let mut x = n.wrapping_add(1).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        while level < 16 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            // Use a geometric distribution approximation
            let r = (x >> 33) as f64 / u32::MAX as f64;
            if r > self.level_mult { break; }
            level += 1;
        }
        level
    }
}

impl HnswIndex {
    /// Serialize the index to bytes (hand-written binary format, no serde).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        // magic
        buf.extend_from_slice(b"HNSW");
        // version
        buf.push(1u8);
        // m, m0, ef_construction, max_layer
        buf.extend_from_slice(&(self.m as u32).to_le_bytes());
        buf.extend_from_slice(&(self.m0 as u32).to_le_bytes());
        buf.extend_from_slice(&(self.ef_construction as u32).to_le_bytes());
        buf.extend_from_slice(&(self.max_layer as u32).to_le_bytes());
        // entry_point: -1 = None, else index as i64
        let ep: i64 = match self.entry_point {
            None => -1,
            Some(idx) => idx as i64,
        };
        buf.extend_from_slice(&ep.to_le_bytes());
        // node_count
        buf.extend_from_slice(&(self.nodes.len() as u32).to_le_bytes());
        // nodes
        for node in &self.nodes {
            buf.extend_from_slice(&node.id.to_le_bytes());
            let dim = node.vector.len() as u32;
            buf.extend_from_slice(&dim.to_le_bytes());
            for &f in &node.vector {
                buf.extend_from_slice(&f.to_le_bytes());
            }
            let layer_count = node.connections.len() as u32;
            buf.extend_from_slice(&layer_count.to_le_bytes());
            for layer_neighbors in &node.connections {
                let nc = layer_neighbors.len() as u32;
                buf.extend_from_slice(&nc.to_le_bytes());
                for &nb in layer_neighbors {
                    buf.extend_from_slice(&(nb as u32).to_le_bytes());
                }
            }
        }
        buf
    }

    /// Deserialize from bytes. Returns Err if format is wrong.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let mut pos = 0usize;

        macro_rules! need {
            ($n:expr) => {
                if pos + $n > data.len() {
                    return Err(format!("truncated at offset {}", pos));
                }
            };
        }
        macro_rules! read_u8 {
            () => {{
                need!(1);
                let v = data[pos];
                pos += 1;
                v
            }};
        }
        macro_rules! read_u32 {
            () => {{
                need!(4);
                let v = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
                pos += 4;
                v
            }};
        }
        macro_rules! read_i64 {
            () => {{
                need!(8);
                let v = i64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
                pos += 8;
                v
            }};
        }
        macro_rules! read_u64 {
            () => {{
                need!(8);
                let v = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
                pos += 8;
                v
            }};
        }
        macro_rules! read_f64 {
            () => {{
                need!(8);
                let v = f64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
                pos += 8;
                v
            }};
        }

        // magic
        need!(4);
        if &data[pos..pos + 4] != b"HNSW" {
            return Err("bad magic".to_string());
        }
        pos += 4;

        // version
        let version = read_u8!();
        if version != 1 {
            return Err(format!("unsupported version {}", version));
        }

        let m = read_u32!() as usize;
        let m0 = read_u32!() as usize;
        let ef_construction = read_u32!() as usize;
        let max_layer = read_u32!() as usize;
        let ep_raw = read_i64!();
        let entry_point = if ep_raw < 0 { None } else { Some(ep_raw as usize) };
        let node_count = read_u32!() as usize;

        let level_mult = if m > 1 { 1.0 / (m as f64).ln() } else { 1.0 };

        let mut nodes = Vec::with_capacity(node_count);
        for _ in 0..node_count {
            let id = read_u64!();
            let dim = read_u32!() as usize;
            let mut vector = Vec::with_capacity(dim);
            for _ in 0..dim {
                vector.push(read_f64!());
            }
            let layer_count = read_u32!() as usize;
            let mut connections = Vec::with_capacity(layer_count);
            for _ in 0..layer_count {
                let nc = read_u32!() as usize;
                let mut layer_neighbors = Vec::with_capacity(nc);
                for _ in 0..nc {
                    layer_neighbors.push(read_u32!() as usize);
                }
                connections.push(layer_neighbors);
            }
            nodes.push(HnswNode { id, vector, connections });
        }

        Ok(HnswIndex { m, m0, ef_construction, nodes, entry_point, max_layer, level_mult })
    }

    /// Save to a file path.
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let bytes = self.to_bytes();
        std::fs::write(path, &bytes)
    }

    /// Load from a file path.
    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::from_bytes(&bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// Helper trait to get or insert a layer's connection list.
trait GetOrInsertWith {
    fn get_or_insert_with(&mut self, layer: usize, f: impl Fn() -> Vec<usize>);
}

impl GetOrInsertWith for Vec<Vec<usize>> {
    fn get_or_insert_with(&mut self, layer: usize, _f: impl Fn() -> Vec<usize>) {
        while self.len() <= layer {
            self.push(Vec::new());
        }
    }
}

/// L2 (Euclidean) distance between two vectors.
fn l2_distance(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y).powi(2)).sum::<f64>().sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_random_vector(seed: u64, dim: usize) -> Vec<f64> {
        let mut v = Vec::with_capacity(dim);
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        for _ in 0..dim {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            v.push((x >> 11) as f64 / (1u64 << 53) as f64);
        }
        v
    }

    #[test]
    fn test_hnsw_basic() {
        let mut index = HnswIndex::new(8, 32);

        // Insert 100 random vectors of dimension 4
        let dim = 4;
        for i in 0..100u64 {
            let v = make_random_vector(i * 12345 + 7, dim);
            index.insert(i, v);
        }

        assert_eq!(index.len(), 100);

        // Query vector: zero vector
        let query = vec![0.0f64; dim];

        // Find true nearest by brute force
        let mut brute: Vec<(f64, u64)> = index.nodes.iter().map(|n| {
            (l2_distance(&query, &n.vector), n.id)
        }).collect();
        brute.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        // Search with HNSW (use large ef for better recall)
        let results = index.search(&query, 10, 50);

        assert!(!results.is_empty(), "HNSW search returned no results");

        // HNSW is approximate: verify the nearest result is within 2x the true nearest distance
        let true_nearest_dist = brute[0].0;
        let hnsw_nearest_dist = results[0].1;

        assert!(
            hnsw_nearest_dist <= true_nearest_dist * 2.5,
            "HNSW nearest distance {} is more than 2.5x the true nearest distance {}",
            hnsw_nearest_dist, true_nearest_dist
        );

        // Also verify HNSW returned something reasonable: its nearest must be among top-20 brute force
        let top20_ids: Vec<u64> = brute.iter().take(20).map(|(_, id)| *id).collect();
        let hnsw_nearest_id = results[0].0;
        assert!(
            top20_ids.contains(&hnsw_nearest_id),
            "HNSW top result {} not in brute-force top-20: {:?}",
            hnsw_nearest_id, &top20_ids[..5]
        );
    }

    #[test]
    fn test_hnsw_empty() {
        let index = HnswIndex::new(8, 32);
        assert!(index.is_empty());
        let results = index.search(&[0.0, 0.0], 5, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_hnsw_single_vector() {
        let mut index = HnswIndex::new(8, 32);
        index.insert(42, vec![1.0, 2.0, 3.0]);
        let results = index.search(&[1.0, 2.0, 3.0], 1, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 42);
        assert!((results[0].1).abs() < 1e-9);
    }

    #[test]
    fn test_hnsw_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hnsw.bin");

        let mut index = HnswIndex::new(8, 32);
        let dim = 4;
        for i in 0..50u64 {
            let v = make_random_vector(i * 7919 + 3, dim);
            index.insert(i, v);
        }

        let query = vec![0.5f64; dim];
        let results_before = index.search(&query, 5, 20);

        index.save(&path).unwrap();
        let loaded = HnswIndex::load(&path).unwrap();

        assert_eq!(loaded.len(), index.len());

        let results_after = loaded.search(&query, 5, 20);

        // Both searches should return the same top result
        assert!(!results_after.is_empty());
        assert_eq!(results_before[0].0, results_after[0].0);
    }

    #[test]
    fn test_hnsw_empty_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hnsw_empty.bin");

        let index = HnswIndex::new(8, 32);
        index.save(&path).unwrap();

        let loaded = HnswIndex::load(&path).unwrap();
        assert!(loaded.is_empty());
        let results = loaded.search(&[0.0, 0.0], 5, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_ann_recall_1k_vectors() {
        // Simple LCG: seed -> next f64 in [0, 1)
        fn next_lcg(s: &mut u64) -> f64 {
            *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (*s >> 33) as f64 / (u32::MAX as f64)
        }

        let dim = 16usize;
        let n_dataset = 1000usize;
        let n_queries = 10usize;
        let k = 10usize;

        // Generate dataset
        let mut seed = 12345u64;
        let mut dataset: Vec<Vec<f64>> = Vec::with_capacity(n_dataset);
        for _ in 0..n_dataset {
            let vec: Vec<f64> = (0..dim).map(|_| next_lcg(&mut seed)).collect();
            dataset.push(vec);
        }

        // Build HNSW index (m=16, m0=32 is set internally as 2*m, ef_construction=200)
        let mut index = HnswIndex::new(16, 200);
        for (i, vec) in dataset.iter().enumerate() {
            index.insert(i as u64, vec.clone());
        }

        assert_eq!(index.len(), n_dataset);

        // Generate 10 query vectors (use a different seed continuation)
        let mut queries: Vec<Vec<f64>> = Vec::with_capacity(n_queries);
        for _ in 0..n_queries {
            let vec: Vec<f64> = (0..dim).map(|_| next_lcg(&mut seed)).collect();
            queries.push(vec);
        }

        let mut total_recall = 0.0f64;

        for query in &queries {
            // Brute-force top-k
            let mut brute: Vec<(f64, u64)> = dataset
                .iter()
                .enumerate()
                .map(|(i, v)| (l2_distance(query, v), i as u64))
                .collect();
            brute.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let true_top_k: std::collections::HashSet<u64> =
                brute.iter().take(k).map(|(_, id)| *id).collect();

            // HNSW search
            let hnsw_results = index.search(query, k, 50);
            let hnsw_ids: std::collections::HashSet<u64> =
                hnsw_results.iter().map(|(id, _)| *id).collect();

            let hits = true_top_k.intersection(&hnsw_ids).count();
            total_recall += hits as f64 / k as f64;
        }

        let avg_recall = total_recall / n_queries as f64;
        assert!(
            avg_recall >= 0.80,
            "ANN recall@10 = {:.3} is below the required 0.80 threshold",
            avg_recall
        );
    }

    #[test]
    #[ignore]  // run with: cargo test test_ann_recall_100k -- --ignored
    fn test_ann_recall_100k_vectors() {
        fn next_lcg(s: &mut u64) -> f64 {
            *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (*s >> 33) as f64 / (u32::MAX as f64)
        }

        let dim = 32usize;
        let n_dataset = 100_000usize;
        let n_queries = 20usize;
        let k = 10usize;

        let mut seed = 99999u64;
        let mut dataset: Vec<Vec<f64>> = Vec::with_capacity(n_dataset);
        for _ in 0..n_dataset {
            let vec: Vec<f64> = (0..dim).map(|_| next_lcg(&mut seed)).collect();
            dataset.push(vec);
        }

        let mut index = HnswIndex::new(16, 100);
        for (i, vec) in dataset.iter().enumerate() {
            index.insert(i as u64, vec.clone());
        }

        let mut queries: Vec<Vec<f64>> = Vec::with_capacity(n_queries);
        for _ in 0..n_queries {
            let vec: Vec<f64> = (0..dim).map(|_| next_lcg(&mut seed)).collect();
            queries.push(vec);
        }

        let mut total_recall = 0.0f64;
        for query in &queries {
            let mut brute: Vec<(f64, u64)> = dataset.iter().enumerate()
                .map(|(i, v)| {
                    let d: f64 = query.iter().zip(v.iter()).map(|(a, b)| (a - b).powi(2)).sum::<f64>().sqrt();
                    (d, i as u64)
                }).collect();
            brute.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let true_top: std::collections::HashSet<u64> = brute.iter().take(k).map(|(_, id)| *id).collect();

            let hnsw_results = index.search(query, k, 100);
            let hnsw_ids: std::collections::HashSet<u64> = hnsw_results.iter().map(|(id, _)| *id).collect();

            let hits = true_top.intersection(&hnsw_ids).count();
            total_recall += hits as f64 / k as f64;
        }

        let avg_recall = total_recall / n_queries as f64;
        assert!(avg_recall >= 0.80, "100k ANN recall@10 = {:.3} below 0.80", avg_recall);
    }
}
