/// Shard routing: maps key ranges to shard IDs (node groups).
use crate::node::NodeId;
use std::collections::HashMap;

pub type ShardId = u64;

#[derive(Debug, Clone)]
pub struct ShardRange {
    pub shard_id: ShardId,
    /// Inclusive lower bound.
    pub start_key: Vec<u8>,
    /// Exclusive upper bound; empty means unbounded (covers everything from start_key onward).
    pub end_key: Vec<u8>,
}

pub struct ShardRouter {
    /// Sorted by start_key.
    shards: Vec<ShardRange>,
    /// shard_id -> replica node IDs
    shard_nodes: HashMap<ShardId, Vec<NodeId>>,
}

impl ShardRouter {
    pub fn new() -> Self {
        ShardRouter {
            shards: Vec::new(),
            shard_nodes: HashMap::new(),
        }
    }

    /// Add a shard covering [start_key, end_key).
    pub fn add_shard(&mut self, shard: ShardRange, nodes: Vec<NodeId>) {
        let id = shard.shard_id;
        // Insert in sorted order by start_key
        let pos = self.shards.partition_point(|s| s.start_key < shard.start_key);
        self.shards.insert(pos, shard);
        self.shard_nodes.insert(id, nodes);
    }

    /// Find the shard responsible for a given key.
    /// Returns the shard whose [start_key, end_key) covers `key`.
    pub fn route(&self, key: &[u8]) -> Option<ShardId> {
        // Binary search: find the last shard where start_key <= key
        let pos = self.shards.partition_point(|s| s.start_key.as_slice() <= key);
        if pos == 0 {
            return None;
        }
        let candidate = &self.shards[pos - 1];
        // Check end_key: empty end_key means unbounded
        if candidate.end_key.is_empty() || key < candidate.end_key.as_slice() {
            Some(candidate.shard_id)
        } else {
            None
        }
    }

    /// Return the replica nodes for a given shard.
    pub fn nodes_for_shard(&self, shard_id: ShardId) -> &[NodeId] {
        self.shard_nodes.get(&shard_id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Split a shard at a given key, producing two shards:
    ///   - Original shard: [original.start_key, split_key)
    ///   - New shard: [split_key, original.end_key)
    pub fn split_shard(
        &mut self,
        shard_id: ShardId,
        split_key: Vec<u8>,
        new_shard_id: ShardId,
        new_nodes: Vec<NodeId>,
    ) {
        let pos = match self.shards.iter().position(|s| s.shard_id == shard_id) {
            Some(p) => p,
            None => return,
        };

        let original_end_key = self.shards[pos].end_key.clone();

        // Shrink original shard to [start_key, split_key)
        self.shards[pos].end_key = split_key.clone();

        // Create new shard [split_key, original_end_key)
        let new_shard = ShardRange {
            shard_id: new_shard_id,
            start_key: split_key.clone(),
            end_key: original_end_key,
        };

        // Insert the new shard in sorted order
        let insert_pos = self.shards.partition_point(|s| s.start_key.as_slice() <= split_key.as_slice());
        self.shards.insert(insert_pos, new_shard);
        self.shard_nodes.insert(new_shard_id, new_nodes);
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Return all shard IDs whose range overlaps with [low, high].
    /// Overlap condition: shard.start_key <= high AND (shard.end_key is empty OR shard.end_key >= low).
    pub fn find_shards_in_range(&self, low: &[u8], high: &[u8]) -> Vec<ShardId> {
        self.shards
            .iter()
            .filter(|s| {
                let start_ok = s.start_key.as_slice() <= high;
                let end_ok = s.end_key.is_empty() || s.end_key.as_slice() >= low;
                start_ok && end_ok
            })
            .map(|s| s.shard_id)
            .collect()
    }
}

impl Default for ShardRouter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// ShardResult and ShardExecutor
// ---------------------------------------------------------------------------

pub struct ShardResult {
    pub shard_id: ShardId,
    pub rows: Vec<Vec<String>>,
    pub error: Option<String>,
}

pub struct ShardExecutor {
    pub router: ShardRouter,
    pub shard_addresses: std::collections::HashMap<ShardId, String>,
}

impl ShardExecutor {
    pub fn new(router: ShardRouter) -> Self {
        ShardExecutor {
            router,
            shard_addresses: std::collections::HashMap::new(),
        }
    }

    pub fn add_shard_address(&mut self, shard_id: ShardId, address: String) {
        self.shard_addresses.insert(shard_id, address);
    }

    /// Execute a SQL query on all shards that hold data in key_range [low, high].
    /// If key_range is None, fan out to ALL shards.
    /// Results are collected and merged (union of rows).
    pub fn fanout_query(&self, sql: &str, key_range: Option<(&[u8], &[u8])>) -> Vec<ShardResult> {
        let shard_ids: Vec<ShardId> = match key_range {
            None => self.router.shards.iter().map(|s| s.shard_id).collect(),
            Some((low, high)) => self.router.find_shards_in_range(low, high),
        };

        shard_ids
            .into_iter()
            .map(|shard_id| {
                let addr = match self.shard_addresses.get(&shard_id) {
                    Some(a) => a.clone(),
                    None => {
                        return ShardResult {
                            shard_id,
                            rows: Vec::new(),
                            error: Some(format!("no address registered for shard {}", shard_id)),
                        };
                    }
                };
                Self::query_shard(shard_id, &addr, sql)
            })
            .collect()
    }

    fn query_shard(shard_id: ShardId, addr: &str, sql: &str) -> ShardResult {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        let stream = match TcpStream::connect_timeout(
            &addr.parse().unwrap_or_else(|_| "127.0.0.1:1".parse().unwrap()),
            Duration::from_secs(1),
        ) {
            Ok(s) => s,
            Err(e) => {
                return ShardResult {
                    shard_id,
                    rows: Vec::new(),
                    error: Some(format!("connect failed: {}", e)),
                };
            }
        };

        let mut writer = match stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                return ShardResult {
                    shard_id,
                    rows: Vec::new(),
                    error: Some(format!("stream clone failed: {}", e)),
                };
            }
        };

        let send_str = format!("{}\n", sql);
        if let Err(e) = writer.write_all(send_str.as_bytes()) {
            return ShardResult {
                shard_id,
                rows: Vec::new(),
                error: Some(format!("write failed: {}", e)),
            };
        }

        let mut rows = Vec::new();
        let reader = BufReader::new(stream);
        for line_result in reader.lines() {
            match line_result {
                Ok(line) => {
                    if line == "." {
                        break;
                    }
                    let cols: Vec<String> = line.split('\t').map(|s| s.to_string()).collect();
                    rows.push(cols);
                }
                Err(e) => {
                    return ShardResult {
                        shard_id,
                        rows: Vec::new(),
                        error: Some(format!("read failed: {}", e)),
                    };
                }
            }
        }

        ShardResult {
            shard_id,
            rows,
            error: None,
        }
    }

    /// Merge ShardResults into a flat list of rows, dropping error shards.
    pub fn merge_results(results: Vec<ShardResult>) -> Vec<Vec<String>> {
        results
            .into_iter()
            .filter(|r| r.error.is_none())
            .flat_map(|r| r.rows)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_route_single_shard() {
        let mut router = ShardRouter::new();
        router.add_shard(
            ShardRange { shard_id: 1, start_key: vec![0x00], end_key: vec![] },
            vec![1, 2, 3],
        );

        // All keys should route to shard 1
        assert_eq!(router.route(&[0x00]), Some(1));
        assert_eq!(router.route(&[0x50]), Some(1));
        assert_eq!(router.route(&[0xFF]), Some(1));
        assert_eq!(router.route(&[0x00, 0x01]), Some(1));
    }

    #[test]
    fn test_route_two_shards() {
        let mut router = ShardRouter::new();
        // Shard 1: [0x00, 0x80)
        router.add_shard(
            ShardRange { shard_id: 1, start_key: vec![0x00], end_key: vec![0x80] },
            vec![1],
        );
        // Shard 2: [0x80, unbounded)
        router.add_shard(
            ShardRange { shard_id: 2, start_key: vec![0x80], end_key: vec![] },
            vec![2],
        );

        // Keys below 0x80 go to shard 1
        assert_eq!(router.route(&[0x00]), Some(1));
        assert_eq!(router.route(&[0x7F]), Some(1));
        // Boundary key 0x80 goes to shard 2
        assert_eq!(router.route(&[0x80]), Some(2));
        assert_eq!(router.route(&[0xFF]), Some(2));
    }

    #[test]
    fn test_split_shard() {
        let mut router = ShardRouter::new();
        // One shard covering everything
        router.add_shard(
            ShardRange { shard_id: 1, start_key: vec![0x00], end_key: vec![] },
            vec![1, 2],
        );
        assert_eq!(router.shard_count(), 1);

        // Split at 0x80: shard 1 gets [0x00, 0x80), new shard 2 gets [0x80, unbounded)
        router.split_shard(1, vec![0x80], 2, vec![3, 4]);
        assert_eq!(router.shard_count(), 2);

        // Verify routing after split
        assert_eq!(router.route(&[0x00]), Some(1));
        assert_eq!(router.route(&[0x7F]), Some(1));
        assert_eq!(router.route(&[0x80]), Some(2));
        assert_eq!(router.route(&[0xFF]), Some(2));

        // Verify nodes
        assert_eq!(router.nodes_for_shard(1), &[1, 2]);
        assert_eq!(router.nodes_for_shard(2), &[3, 4]);
    }

    #[test]
    fn test_route_empty() {
        let router = ShardRouter::new();
        assert_eq!(router.route(&[0x00]), None);
        assert_eq!(router.route(&[0xFF]), None);
    }

    #[test]
    fn test_find_shards_in_range() {
        let mut router = ShardRouter::new();
        // Shard 0: [0, 51) covers keys 0..=50
        router.add_shard(
            ShardRange { shard_id: 0, start_key: vec![0], end_key: vec![51] },
            vec![1],
        );
        // Shard 1: [51, unbounded) covers keys 51..
        router.add_shard(
            ShardRange { shard_id: 1, start_key: vec![51], end_key: vec![] },
            vec![2],
        );

        // Query range [25, 75] overlaps both shards
        let both = router.find_shards_in_range(&[25], &[75]);
        assert_eq!(both.len(), 2, "both shards should overlap [25,75]");

        // Query range [0, 30] overlaps only shard 0
        let only_zero = router.find_shards_in_range(&[0], &[30]);
        assert_eq!(only_zero.len(), 1, "only shard 0 should overlap [0,30]");
        assert_eq!(only_zero[0], 0);
    }

    #[test]
    fn test_fanout_all_shards() {
        let mut router = ShardRouter::new();
        // 3 shards with non-overlapping ranges
        router.add_shard(
            ShardRange { shard_id: 0, start_key: vec![0], end_key: vec![34] },
            vec![1],
        );
        router.add_shard(
            ShardRange { shard_id: 1, start_key: vec![34], end_key: vec![67] },
            vec![2],
        );
        router.add_shard(
            ShardRange { shard_id: 2, start_key: vec![67], end_key: vec![] },
            vec![3],
        );

        let mut executor = ShardExecutor::new(router);
        // Register unreachable addresses so connect will fail gracefully
        executor.add_shard_address(0, "127.0.0.1:1".to_string());
        executor.add_shard_address(1, "127.0.0.1:1".to_string());
        executor.add_shard_address(2, "127.0.0.1:1".to_string());

        let results = executor.fanout_query("SELECT 1", None);
        // All 3 shards attempted
        assert_eq!(results.len(), 3);
        // All should have errors since port 1 is unreachable
        for r in &results {
            assert!(r.error.is_some(), "shard {} should have an error", r.shard_id);
        }
        // merge_results drops errored shards -> empty
        let merged = ShardExecutor::merge_results(results);
        assert!(merged.is_empty(), "merged results should be empty when all shards errored");
    }
}
