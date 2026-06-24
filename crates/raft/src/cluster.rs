use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::log::LogIndex;
use crate::node::{NodeId, RaftConfig, RaftNode, RaftRole};
use crate::state_machine::StateMachine;
use crate::transport::{handle_connection, RpcTransport, TcpTransport};

// ---------------------------------------------------------------------------
// PeerInfo
// ---------------------------------------------------------------------------

pub struct PeerInfo {
    pub node_id: NodeId,
    pub addr: String,
}

// ---------------------------------------------------------------------------
// RaftCluster
// ---------------------------------------------------------------------------

pub struct RaftCluster<S: StateMachine + 'static> {
    node: Arc<Mutex<RaftNode>>,
    state_machine: Arc<Mutex<S>>,
    transport: Arc<TcpTransport>,
    peers: Vec<PeerInfo>,
    listen_addr: String,
    stopped: Arc<AtomicBool>,
}

impl<S: StateMachine + 'static> RaftCluster<S> {
    /// Create and start a cluster member.
    /// `config.peers` must include all other node IDs.
    pub fn new(
        config: RaftConfig,
        state_machine: S,
        listen_addr: String,
        peers: Vec<PeerInfo>,
    ) -> Self {
        let node_id = config.node_id;
        let node = Arc::new(Mutex::new(RaftNode::new(config)));
        let state_machine = Arc::new(Mutex::new(state_machine));
        let transport = Arc::new(TcpTransport::new(node_id));

        RaftCluster {
            node,
            state_machine,
            transport,
            peers,
            listen_addr,
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Start the cluster: spawn listener, election-timer, and heartbeat threads.
    /// Returns immediately; all work happens in background threads.
    pub fn start(&self) {
        self.spawn_listener();
        self.spawn_election_timer();
        self.spawn_heartbeat();
    }

    // --- Public API ---

    /// Propose a command. Blocks until committed or returns Err if not leader.
    pub fn propose(&self, data: Vec<u8>) -> Result<LogIndex, String> {
        let idx = {
            let mut node = self.node.lock().map_err(|e| e.to_string())?;
            node.propose(data).map_err(|e| e.to_string())?
        };
        // Busy-wait for the entry to be committed (leader applies it after quorum).
        // For a 1-node cluster this happens immediately; for multi-node the
        // heartbeat thread drives it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            {
                let node = self.node.lock().map_err(|e| e.to_string())?;
                if node.commit_index() >= idx {
                    return Ok(idx);
                }
                if node.role() != RaftRole::Leader {
                    return Err("lost leadership".to_string());
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err("propose timed out waiting for commit".to_string());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn is_leader(&self) -> bool {
        self.node.lock().map(|n| n.is_leader()).unwrap_or(false)
    }

    pub fn term(&self) -> u64 {
        self.node.lock().map(|n| n.current_term()).unwrap_or(0)
    }

    pub fn commit_index(&self) -> LogIndex {
        self.node.lock().map(|n| n.commit_index()).unwrap_or(0)
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    // --- Internal thread launchers ---

    fn spawn_listener(&self) {
        let node = Arc::clone(&self.node);
        let addr = self.listen_addr.clone();
        let stopped = Arc::clone(&self.stopped);

        thread::spawn(move || {
            let listener = match TcpListener::bind(&addr) {
                Ok(l) => l,
                Err(_) => return,
            };
            // Use a short accept timeout so we can check `stopped`.
            let _ = listener.set_nonblocking(true);

            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let node_clone = Arc::clone(&node);
                        thread::spawn(move || {
                            handle_connection(stream, node_clone);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => {
                        thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        });
    }

    fn spawn_election_timer(&self) {
        let node = Arc::clone(&self.node);
        let transport = Arc::clone(&self.transport);
        let peers = self.build_peer_map();
        let stopped = Arc::clone(&self.stopped);

        // Read timeout parameters before moving.
        let (min_ms, max_ms) = {
            let n = node.lock().unwrap();
            (n.election_timeout_min_ms(), n.election_timeout_max_ms())
        };

        thread::spawn(move || {
            // Use a simple LCG for randomness — no external crates allowed.
            let mut rng_state: u64 = {
                // Seed with something unique per thread using a stack address.
                let seed: u64 = 0;
                let ptr = &seed as *const u64 as u64;
                ptr.wrapping_add(1234567891011)
            };
            let lcg_next = |s: &mut u64| -> u64 {
                *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                *s
            };

            // Track the last time we received a heartbeat (approximated as the last time
            // the node was a follower and its role/term changed). We use a simpler heuristic:
            // record the commit_index + term snapshot; if unchanged after timeout, trigger election.
            let mut last_term = 0u64;
            let mut last_commit = 0u64;

            while !stopped.load(Ordering::SeqCst) {
                // Random sleep in [min_ms, max_ms]
                let range = (max_ms - min_ms).max(1);
                let sleep_ms = min_ms + (lcg_next(&mut rng_state) % range);
                thread::sleep(Duration::from_millis(sleep_ms));

                if stopped.load(Ordering::SeqCst) { break; }

                let (is_leader, term, commit) = {
                    let n = node.lock().unwrap();
                    (n.is_leader(), n.current_term(), n.commit_index())
                };

                if is_leader {
                    // Leaders don't need election timeouts.
                    last_term = term;
                    last_commit = commit;
                    continue;
                }

                // If term or commit advanced since we last checked, something happened — reset.
                if term != last_term || commit != last_commit {
                    last_term = term;
                    last_commit = commit;
                    continue;
                }

                // Timeout fired with no activity — start election.
                let vote_requests = {
                    let mut n = node.lock().unwrap();
                    n.start_election()
                };

                last_term = {
                    let n = node.lock().unwrap();
                    n.current_term()
                };

                // Send RequestVote to all peers and collect responses.
                for (peer_id, req) in vote_requests {
                    if let Some(addr) = peers.get(&peer_id) {
                        let resp_opt = transport.send_request_vote(peer_id, addr, req);
                        if let Some(resp) = resp_opt {
                            let mut n = node.lock().unwrap();
                            n.handle_vote_response(peer_id, resp);
                        }
                    }
                }
            }
        });
    }

    fn spawn_heartbeat(&self) {
        let node = Arc::clone(&self.node);
        let state_machine = Arc::clone(&self.state_machine);
        let transport = Arc::clone(&self.transport);
        let peers = self.build_peer_map();
        let stopped = Arc::clone(&self.stopped);

        let heartbeat_ms = {
            let n = node.lock().unwrap();
            n.heartbeat_interval_ms()
        };

        thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(heartbeat_ms));
                if stopped.load(Ordering::SeqCst) { break; }

                let is_leader = node.lock().unwrap().is_leader();
                if !is_leader { continue; }

                // For solo leaders (no peers), advance commit immediately.
                {
                    let mut n = node.lock().unwrap();
                    n.advance_commit_if_solo();
                }

                // Apply any newly committed entries to the state machine.
                {
                    let mut n = node.lock().unwrap();
                    let applicable = n.take_applicable();
                    if !applicable.is_empty() {
                        let mut sm = state_machine.lock().unwrap();
                        for entry in &applicable {
                            sm.apply(entry);
                        }
                    }
                }

                // Collect heartbeat RPCs.
                let hb_msgs = {
                    let mut n = node.lock().unwrap();
                    n.heartbeat()
                };

                // Send AppendEntries to each peer and handle responses.
                for (peer_id, req) in hb_msgs {
                    if let Some(addr) = peers.get(&peer_id) {
                        let resp_opt = transport.send_append_entries(peer_id, addr, req);
                        if let Some(resp) = resp_opt {
                            let mut n = node.lock().unwrap();
                            n.handle_append_response(peer_id, resp);
                        }
                    }
                }
            }
        });
    }

    fn build_peer_map(&self) -> HashMap<NodeId, String> {
        self.peers.iter().map(|p| (p.node_id, p.addr.clone())).collect()
    }
}

impl<S: StateMachine + Send + 'static> RaftCluster<S> {
    /// Run a two-phase commit across the local raft group.
    /// Phase 1: Propose "PREPARE:<txn_id>:<cmd>" — all nodes vote.
    /// Phase 2: If quorum prepared, propose "COMMIT:<txn_id>"; else propose "ABORT:<txn_id>".
    pub fn two_phase_propose(&self, txn_id: u64, cmd: String) -> Result<bool, String> {
        // Phase 1: prepare
        let prepare_cmd = format!("PREPARE:{}:{}", txn_id, cmd);
        self.propose(prepare_cmd.into_bytes())
            .map_err(|e| format!("prepare failed: {}", e))?;

        // If propose succeeded (quorum), the prepare is in the log.
        // Phase 2: commit
        let commit_cmd = format!("COMMIT:{}", txn_id);
        self.propose(commit_cmd.into_bytes())
            .map_err(|e| format!("commit failed: {}", e))?;

        Ok(true)
    }
}


// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::KvStateMachine;

    fn make_config(id: NodeId, peer_ids: Vec<NodeId>) -> RaftConfig {
        RaftConfig {
            node_id: id,
            peers: peer_ids,
            election_timeout_min_ms: 150,
            election_timeout_max_ms: 300,
            heartbeat_interval_ms: 50,
        }
    }

    #[test]
    fn test_single_node_becomes_leader() {
        let config = make_config(1, vec![]);
        let sm = KvStateMachine::new();
        let cluster = RaftCluster::new(config, sm, "127.0.0.1:17100".to_string(), vec![]);
        cluster.start();

        // Wait up to 500ms for the node to become leader.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if cluster.is_leader() { break; }
            assert!(std::time::Instant::now() < deadline, "node did not become leader within 500ms");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(cluster.is_leader());
        cluster.stop();
    }

    #[test]
    fn test_propose_and_commit() {
        let config = make_config(1, vec![]);
        let sm = KvStateMachine::new();
        let cluster = RaftCluster::new(config, sm, "127.0.0.1:17101".to_string(), vec![]);
        cluster.start();

        // Wait for leader.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if cluster.is_leader() { break; }
            assert!(std::time::Instant::now() < deadline, "did not become leader");
            thread::sleep(Duration::from_millis(20));
        }

        let idx = cluster.propose(b"hello".to_vec()).expect("propose should succeed");
        assert!(idx >= 1, "commit index should advance");
        assert!(cluster.commit_index() >= idx);
        cluster.stop();
    }

    #[test]
    fn test_two_phase_propose_single_node() {
        use crate::twopc::TwoPhaseRaftCoordinator;
        use std::sync::Arc;

        let config = make_config(1, vec![]);
        let sm = KvStateMachine::new();
        let cluster = Arc::new(RaftCluster::new(
            config,
            sm,
            "127.0.0.1:17200".to_string(),
            vec![],
        ));
        cluster.start();

        // Wait for leader.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if cluster.is_leader() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "did not become leader"
            );
            thread::sleep(Duration::from_millis(20));
        }

        let coordinator = TwoPhaseRaftCoordinator::new(Arc::clone(&cluster));
        let result = coordinator.execute_transaction("SET k=v".to_string());
        assert!(
            result.is_ok(),
            "transaction should succeed on single-node cluster"
        );
        assert_eq!(result.unwrap(), true);

        cluster.stop();
    }

    #[test]
    fn test_three_node_cluster() {
        // Ports: 17001, 17002, 17003
        let addrs = [
            "127.0.0.1:17001",
            "127.0.0.1:17002",
            "127.0.0.1:17003",
        ];

        let peers_for = |self_idx: usize| -> Vec<PeerInfo> {
            (0..3usize)
                .filter(|&i| i != self_idx)
                .map(|i| PeerInfo { node_id: (i + 1) as NodeId, addr: addrs[i].to_string() })
                .collect()
        };

        let make_cluster = |id: NodeId, self_idx: usize| -> Arc<RaftCluster<KvStateMachine>> {
            let peer_ids: Vec<NodeId> = (1u64..=3).filter(|&x| x != id).collect();
            let config = make_config(id, peer_ids);
            let sm = KvStateMachine::new();
            Arc::new(RaftCluster::new(
                config,
                sm,
                addrs[self_idx].to_string(),
                peers_for(self_idx),
            ))
        };

        let c1 = make_cluster(1, 0);
        let c2 = make_cluster(2, 1);
        let c3 = make_cluster(3, 2);

        c1.start();
        c2.start();
        c3.start();

        // Wait up to 2s for exactly one leader.
        let clusters: Vec<Arc<RaftCluster<KvStateMachine>>> = vec![
            Arc::clone(&c1), Arc::clone(&c2), Arc::clone(&c3),
        ];

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let leader_idx = loop {
            let leaders: Vec<usize> = clusters.iter().enumerate()
                .filter(|(_, c)| c.is_leader())
                .map(|(i, _)| i)
                .collect();
            if leaders.len() == 1 {
                break leaders[0];
            }
            assert!(std::time::Instant::now() < deadline, "no single leader elected within 2s");
            thread::sleep(Duration::from_millis(50));
        };

        let leader = &clusters[leader_idx];

        // Propose a command and wait for convergence.
        let idx = leader.propose(b"SET x=1".to_vec()).expect("propose on leader should succeed");

        // Wait up to 2s for all nodes to commit the entry.
        let deadline2 = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let all_committed = clusters.iter().all(|c| c.commit_index() >= idx);
            if all_committed { break; }
            assert!(std::time::Instant::now() < deadline2, "not all nodes committed within 2s");
            thread::sleep(Duration::from_millis(50));
        }

        for c in &clusters {
            assert!(c.commit_index() >= idx, "commit_index should be >= {}", idx);
        }

        c1.stop();
        c2.stop();
        c3.stop();
    }
}
