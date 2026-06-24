use crate::log::{LogIndex, RaftLog, Snapshot, Term};
use crate::rpc::{AppendEntriesReq, AppendEntriesResp, InstallSnapshotReq, InstallSnapshotResp, RequestVoteReq, RequestVoteResp};

pub type NodeId = u64;

/// The role of a RAFT node.
#[derive(Debug, Clone, PartialEq)]
pub enum RaftRole {
    Leader,
    Follower,
    Candidate,
}

/// Configuration for a RAFT node.
pub struct RaftConfig {
    pub node_id: NodeId,
    pub peers: Vec<NodeId>,
    pub election_timeout_min_ms: u64,
    pub election_timeout_max_ms: u64,
    pub heartbeat_interval_ms: u64,
}

/// A RAFT consensus node.
pub struct RaftNode {
    pub config: RaftConfig,
    pub role: RaftRole,

    // Persistent state (would be written to stable storage in a real implementation)
    current_term: Term,
    voted_for: Option<NodeId>,
    log: RaftLog,

    // Volatile state
    commit_index: LogIndex,
    #[allow(dead_code)]
    last_applied: LogIndex,

    // Leader volatile state — per-peer tracking
    /// next_index[i] = next log index to send to peer config.peers[i]
    next_index: Vec<(NodeId, LogIndex)>,
    /// match_index[i] = highest log index known replicated on peer config.peers[i]
    match_index: Vec<(NodeId, LogIndex)>,

    // Candidate state
    votes_received: usize,
}

impl RaftNode {
    pub fn new(config: RaftConfig) -> Self {
        let peers = config.peers.clone();
        RaftNode {
            config,
            role: RaftRole::Follower,
            current_term: 0,
            voted_for: None,
            log: RaftLog::new(),
            commit_index: 0,
            last_applied: 0,
            next_index: peers.iter().map(|&p| (p, 1)).collect(),
            match_index: peers.iter().map(|&p| (p, 0)).collect(),
            votes_received: 0,
        }
    }

    // --- Public state accessors ---

    pub fn is_leader(&self) -> bool {
        self.role == RaftRole::Leader
    }

    pub fn role(&self) -> RaftRole {
        self.role.clone()
    }

    pub fn current_term(&self) -> Term {
        self.current_term
    }

    pub fn commit_index(&self) -> LogIndex {
        self.commit_index
    }

    pub fn election_timeout_min_ms(&self) -> u64 {
        self.config.election_timeout_min_ms
    }

    pub fn election_timeout_max_ms(&self) -> u64 {
        self.config.election_timeout_max_ms
    }

    pub fn heartbeat_interval_ms(&self) -> u64 {
        self.config.heartbeat_interval_ms
    }

    // --- RPC handlers ---

    /// Process an incoming AppendEntries RPC. Returns the response.
    pub fn handle_append_entries(&mut self, req: AppendEntriesReq) -> AppendEntriesResp {
        // Rule 1: Reply false if term < currentTerm
        if req.term < self.current_term {
            return AppendEntriesResp {
                term: self.current_term,
                success: false,
                match_index: self.log.last_index(),
            };
        }

        // If we see a higher term or a valid leader, step down to follower
        if req.term > self.current_term {
            self.current_term = req.term;
            self.voted_for = None;
        }
        self.role = RaftRole::Follower;

        // Rule 2: Reply false if log doesn't contain an entry at prevLogIndex with prevLogTerm
        if req.prev_log_index > 0 {
            match self.log.get(req.prev_log_index) {
                None => {
                    return AppendEntriesResp {
                        term: self.current_term,
                        success: false,
                        match_index: self.log.last_index(),
                    };
                }
                Some(entry) if entry.term != req.prev_log_term => {
                    // Conflict: truncate from this point
                    self.log.truncate_after(req.prev_log_index.saturating_sub(1));
                    return AppendEntriesResp {
                        term: self.current_term,
                        success: false,
                        match_index: self.log.last_index(),
                    };
                }
                _ => {}
            }
        }

        // Rule 3 & 4: Append new entries, resolving conflicts
        for entry in &req.entries {
            if let Some(existing) = self.log.get(entry.index) {
                if existing.term != entry.term {
                    // Conflict: truncate and append from here
                    self.log.truncate_after(entry.index.saturating_sub(1));
                    self.log.append(entry.term, entry.data.clone());
                }
                // If terms match, entry already present — skip
            } else {
                // Entry beyond end of log: append
                self.log.append(entry.term, entry.data.clone());
            }
        }

        // Rule 5: Update commit index
        if req.leader_commit > self.commit_index {
            let new_commit = req.leader_commit.min(self.log.last_index());
            self.commit_index = new_commit;
            self.log.advance_commit(new_commit);
        }

        AppendEntriesResp {
            term: self.current_term,
            success: true,
            match_index: self.log.last_index(),
        }
    }

    /// Process an incoming RequestVote RPC. Returns the response.
    pub fn handle_request_vote(&mut self, req: RequestVoteReq) -> RequestVoteResp {
        // Reply false if term < currentTerm
        if req.term < self.current_term {
            return RequestVoteResp {
                term: self.current_term,
                vote_granted: false,
            };
        }

        // If request has higher term, update and become follower
        if req.term > self.current_term {
            self.current_term = req.term;
            self.voted_for = None;
            self.role = RaftRole::Follower;
        }

        // Grant vote if (have not voted OR voted for this candidate)
        // AND candidate's log is at least as up-to-date as ours
        let can_vote = self.voted_for.is_none() || self.voted_for == Some(req.candidate_id);
        let log_ok = req.last_log_term > self.log.last_term()
            || (req.last_log_term == self.log.last_term()
                && req.last_log_index >= self.log.last_index());

        if can_vote && log_ok {
            self.voted_for = Some(req.candidate_id);
            RequestVoteResp { term: self.current_term, vote_granted: true }
        } else {
            RequestVoteResp { term: self.current_term, vote_granted: false }
        }
    }

    // --- Leader operations ---

    /// Called by the leader to propose a new log entry.
    /// Returns the log index, or Err if this node is not the leader.
    pub fn propose(&mut self, data: Vec<u8>) -> Result<LogIndex, &'static str> {
        if self.role != RaftRole::Leader {
            return Err("not leader");
        }
        let index = self.log.append(self.current_term, data);
        Ok(index)
    }

    /// Generate AppendEntries RPCs to send to all peers (leader heartbeat / replication).
    /// Returns a list of (peer_id, request) pairs.
    pub fn heartbeat(&mut self) -> Vec<(NodeId, AppendEntriesReq)> {
        if self.role != RaftRole::Leader {
            return vec![];
        }
        let term = self.current_term;
        let leader_id = self.config.node_id;
        let leader_commit = self.commit_index;

        self.config.peers.iter().map(|&peer| {
            let next = self.next_index_for(peer);
            let prev_log_index = next.saturating_sub(1);
            let prev_log_term = self.log.get(prev_log_index)
                .map(|e| e.term)
                .unwrap_or(0);
            let entries: Vec<_> = self.log.entries_from(next).to_vec();

            let req = AppendEntriesReq {
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
            };
            (peer, req)
        }).collect()
    }

    /// Record a response from a peer to an AppendEntries RPC.
    pub fn handle_append_response(&mut self, from: NodeId, resp: AppendEntriesResp) {
        if resp.term > self.current_term {
            self.current_term = resp.term;
            self.voted_for = None;
            self.role = RaftRole::Follower;
            return;
        }
        if self.role != RaftRole::Leader {
            return;
        }
        if resp.success {
            // Update next_index and match_index for this peer
            self.set_match_index(from, resp.match_index);
            self.set_next_index(from, resp.match_index + 1);

            // Check if we can advance commit index
            // Find the highest N such that a majority of nodes have match_index >= N
            let quorum = self.config.peers.len().div_ceil(2) + 1; // majority
            let last = self.log.last_index();
            for n in (self.commit_index + 1..=last).rev() {
                if self.log.get(n).map(|e| e.term).unwrap_or(0) != self.current_term {
                    continue;
                }
                // Count nodes (including self) with match_index >= n
                let matched = 1 + self.match_index.iter().filter(|(_, mi)| *mi >= n).count();
                if matched >= quorum {
                    self.commit_index = n;
                    self.log.advance_commit(n);
                    break;
                }
            }
        } else {
            // Decrement nextIndex for this peer (fast rollback using match_index hint)
            let new_next = resp.match_index + 1;
            let current_next = self.next_index_for(from);
            if new_next < current_next {
                self.set_next_index(from, new_next.max(1));
            } else {
                // Fall back to decrement by 1
                self.set_next_index(from, current_next.saturating_sub(1).max(1));
            }
        }
    }

    // --- Election operations ---

    /// Called when an election timeout fires. Transitions to Candidate and starts an election.
    /// Returns the RequestVote messages to send to all peers.
    pub fn start_election(&mut self) -> Vec<(NodeId, RequestVoteReq)> {
        self.current_term += 1;
        self.role = RaftRole::Candidate;
        self.voted_for = Some(self.config.node_id); // vote for self
        self.votes_received = 1; // count self-vote

        // Check if we already have a quorum (e.g., single-node cluster)
        let quorum = self.config.peers.len().div_ceil(2) + 1;
        if self.votes_received >= quorum {
            self.become_leader();
            return vec![];
        }

        let term = self.current_term;
        let candidate_id = self.config.node_id;
        let last_log_index = self.log.last_index();
        let last_log_term = self.log.last_term();

        self.config.peers.iter().map(|&peer| {
            let req = RequestVoteReq {
                term,
                candidate_id,
                last_log_index,
                last_log_term,
            };
            (peer, req)
        }).collect()
    }

    /// Record a vote response from a peer. Returns true if this node became leader.
    pub fn handle_vote_response(&mut self, _from: NodeId, resp: RequestVoteResp) -> bool {
        if resp.term > self.current_term {
            self.current_term = resp.term;
            self.voted_for = None;
            self.role = RaftRole::Follower;
            return false;
        }
        if self.role != RaftRole::Candidate {
            return false;
        }
        if resp.vote_granted {
            self.votes_received += 1;
            let quorum = self.config.peers.len().div_ceil(2) + 1;
            if self.votes_received >= quorum {
                self.become_leader();
                return true;
            }
        }
        false
    }

    // --- Private helpers ---

    fn become_leader(&mut self) {
        self.role = RaftRole::Leader;
        let last = self.log.last_index();
        // Reset next_index to leader's last index + 1 for all peers
        for (_, ni) in &mut self.next_index {
            *ni = last + 1;
        }
        // Reset match_index to 0 for all peers
        for (_, mi) in &mut self.match_index {
            *mi = 0;
        }
    }

    /// Create a snapshot of the state machine at the given index and compact the log.
    pub fn create_snapshot(&mut self, index: LogIndex, snapshot_data: Vec<u8>) {
        let term = self.log.get(index).map(|e| e.term).unwrap_or(self.current_term);
        let snapshot = Snapshot {
            last_included_index: index,
            last_included_term: term,
            data: snapshot_data,
        };
        self.log.compact(snapshot);
    }

    /// Handle an incoming InstallSnapshot RPC (from leader to lagging follower).
    pub fn handle_install_snapshot(&mut self, req: InstallSnapshotReq) -> InstallSnapshotResp {
        // Rule: if request term < currentTerm, reject
        if req.term < self.current_term {
            return InstallSnapshotResp { term: self.current_term };
        }

        // Update term if necessary
        if req.term > self.current_term {
            self.current_term = req.term;
            self.voted_for = None;
        }
        self.role = RaftRole::Follower;

        // Only install if the snapshot is newer than what we have
        if req.last_included_index <= self.log.last_applied() {
            return InstallSnapshotResp { term: self.current_term };
        }

        let snapshot = Snapshot {
            last_included_index: req.last_included_index,
            last_included_term: req.last_included_term,
            data: req.data,
        };
        self.log.compact(snapshot);

        InstallSnapshotResp { term: self.current_term }
    }

    /// Generate InstallSnapshot RPC for a peer that's too far behind (next_index <= snapshot base).
    /// Returns Some((peer_id, req)) if the peer needs a snapshot, None otherwise.
    pub fn maybe_install_snapshot(&self, peer: NodeId) -> Option<(NodeId, InstallSnapshotReq)> {
        if self.role != RaftRole::Leader {
            return None;
        }
        let snap = self.log.snapshot()?;
        let next = self.next_index_for(peer);
        // If the peer's next expected index is at or before the snapshot base, send snapshot
        if next <= snap.last_included_index {
            let req = InstallSnapshotReq {
                term: self.current_term,
                leader_id: self.config.node_id,
                last_included_index: snap.last_included_index,
                last_included_term: snap.last_included_term,
                data: snap.data.clone(),
            };
            Some((peer, req))
        } else {
            None
        }
    }

    /// Advance last_applied to commit_index and return newly applicable log entries.
    /// Intended for the cluster manager to drive the state machine.
    pub fn take_applicable(&mut self) -> Vec<crate::log::LogEntry> {
        self.log.take_applicable()
    }

    /// If this is a leader with no peers, advance the commit index to the log tail
    /// (a single node is always its own quorum).
    pub fn advance_commit_if_solo(&mut self) {
        if self.role != RaftRole::Leader || !self.config.peers.is_empty() {
            return;
        }
        let last = self.log.last_index();
        if last > self.commit_index {
            self.commit_index = last;
            self.log.advance_commit(last);
        }
    }

    fn next_index_for(&self, peer: NodeId) -> LogIndex {
        self.next_index.iter().find(|(p, _)| *p == peer).map(|(_, i)| *i).unwrap_or(1)
    }

    fn set_next_index(&mut self, peer: NodeId, index: LogIndex) {
        if let Some(entry) = self.next_index.iter_mut().find(|(p, _)| *p == peer) {
            entry.1 = index;
        }
    }

    fn set_match_index(&mut self, peer: NodeId, index: LogIndex) {
        if let Some(entry) = self.match_index.iter_mut().find(|(p, _)| *p == peer) {
            entry.1 = index;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(id: NodeId, peers: Vec<NodeId>) -> RaftConfig {
        RaftConfig {
            node_id: id,
            peers,
            election_timeout_min_ms: 150,
            election_timeout_max_ms: 300,
            heartbeat_interval_ms: 50,
        }
    }

    // Helper: make a single-node cluster (no peers).
    fn single_node() -> RaftNode {
        RaftNode::new(make_config(1, vec![]))
    }

    // Helper: make a 3-node cluster.
    fn three_node_cluster() -> (RaftNode, RaftNode, RaftNode) {
        let n1 = RaftNode::new(make_config(1, vec![2, 3]));
        let n2 = RaftNode::new(make_config(2, vec![1, 3]));
        let n3 = RaftNode::new(make_config(3, vec![1, 2]));
        (n1, n2, n3)
    }

    #[test]
    fn test_leader_election_single_node() {
        let mut node = single_node();
        assert_eq!(node.role, RaftRole::Follower);

        // No peers — start election and immediately win (quorum = 1 = self).
        // With peers.len() == 0: quorum = 0.div_ceil(2) + 1 = 1.
        // votes_received = 1 (self-vote) >= quorum = 1 → become_leader() called immediately.
        let msgs = node.start_election();
        assert_eq!(msgs.len(), 0, "no peers to message");
        assert_eq!(node.current_term(), 1, "term should be incremented");
        assert_eq!(node.role, RaftRole::Leader,
            "single-node should become leader immediately after election");
        assert!(node.is_leader());
    }

    #[test]
    fn test_follower_rejects_stale_term() {
        let mut node = RaftNode::new(make_config(1, vec![2, 3]));
        // Advance node to term 5
        node.current_term = 5;

        let req = RequestVoteReq {
            term: 3,
            candidate_id: 2,
            last_log_index: 0,
            last_log_term: 0,
        };
        let resp = node.handle_request_vote(req);
        assert!(!resp.vote_granted, "should reject stale term vote request");
        assert_eq!(resp.term, 5, "should return current term");
    }

    #[test]
    fn test_append_entries_success() {
        let mut leader = RaftNode::new(make_config(1, vec![2]));
        let mut follower = RaftNode::new(make_config(2, vec![1]));

        // Leader becomes leader at term 1
        leader.start_election();
        let resp = leader.handle_vote_response(2, RequestVoteResp { term: 1, vote_granted: true });
        assert!(resp, "leader should win election");

        // Leader proposes an entry
        let idx = leader.propose(b"cmd1".to_vec()).unwrap();
        assert_eq!(idx, 1);

        // Leader sends heartbeat with the entry
        let msgs = leader.heartbeat();
        assert_eq!(msgs.len(), 1);
        let (_, req) = msgs.into_iter().next().unwrap();

        // Follower processes the AppendEntries
        let follower_resp = follower.handle_append_entries(req.clone());
        assert!(follower_resp.success);
        assert_eq!(follower_resp.match_index, 1);

        // Now leader sends commit update (leader_commit = 1)
        let commit_req = AppendEntriesReq {
            term: req.term,
            leader_id: req.leader_id,
            prev_log_index: 1,
            prev_log_term: req.term,
            entries: vec![],
            leader_commit: 1,
        };
        let commit_resp = follower.handle_append_entries(commit_req);
        assert!(commit_resp.success);
        assert_eq!(follower.commit_index(), 1);
    }

    #[test]
    fn test_append_entries_log_mismatch() {
        let mut follower = RaftNode::new(make_config(2, vec![1]));

        // Inject some entries in follower at term 1
        follower.log.append(1, b"old".to_vec());

        // Leader sends AppendEntries claiming prevLogIndex=1, prevLogTerm=2
        // but follower has term 1 at index 1 — mismatch
        let req = AppendEntriesReq {
            term: 2,
            leader_id: 1,
            prev_log_index: 1,
            prev_log_term: 2, // conflicts with follower's term 1
            entries: vec![],
            leader_commit: 0,
        };
        let resp = follower.handle_append_entries(req);
        assert!(!resp.success, "mismatch should cause failure");
    }

    #[test]
    fn test_log_replication_basic() {
        let (mut leader, mut follower, _) = three_node_cluster();

        // Leader wins election (needs 2/3 votes)
        leader.start_election();
        let won = leader.handle_vote_response(2, RequestVoteResp { term: 1, vote_granted: true });
        assert!(won);

        // Propose 5 entries
        for i in 0..5u8 {
            leader.propose(vec![i]).unwrap();
        }

        // Send all entries to follower
        let msgs = leader.heartbeat();
        let (_, req) = msgs.into_iter().find(|(peer, _)| *peer == 2).unwrap();
        let resp = follower.handle_append_entries(req);
        assert!(resp.success);
        assert_eq!(follower.log.last_index(), 5, "follower should have 5 entries");

        // Verify follower log matches leader log
        for i in 1..=5 {
            let le = leader.log.get(i).unwrap();
            let fe = follower.log.get(i).unwrap();
            assert_eq!(le.data, fe.data, "entry {} data mismatch", i);
            assert_eq!(le.term, fe.term, "entry {} term mismatch", i);
        }
    }

    #[test]
    fn test_split_vote() {
        // Two candidates with the same log, neither should win without a quorum
        let (mut n1, mut n2, _n3) = three_node_cluster();

        // Both start elections in the same term
        n1.start_election();
        n2.start_election();

        // n1 requests vote from n2 — but n2 already voted for itself
        let req_from_n1 = RequestVoteReq {
            term: n1.current_term(),
            candidate_id: 1,
            last_log_index: n1.log.last_index(),
            last_log_term: n1.log.last_term(),
        };
        let resp_n2 = n2.handle_request_vote(req_from_n1);
        // n2 has voted for itself, so it won't grant to n1 (assuming same term)
        assert!(!resp_n2.vote_granted, "n2 should not grant vote (already voted)");

        // n1 handles a rejected vote from n2: still only 1 vote (self)
        let became = n1.handle_vote_response(2, RequestVoteResp { term: 1, vote_granted: false });
        assert!(!became, "n1 should not become leader with only 1 vote in 3-node cluster");

        // Neither n1 nor n2 has won
        assert_ne!(n1.role, RaftRole::Leader);
        assert_ne!(n2.role, RaftRole::Leader);
    }

    #[test]
    fn test_commit_requires_quorum() {
        let (mut leader, mut n2, mut n3) = three_node_cluster();

        // Leader wins election
        leader.start_election();
        leader.handle_vote_response(2, RequestVoteResp { term: 1, vote_granted: true });
        assert!(leader.is_leader());

        // Propose an entry
        leader.propose(b"x".to_vec()).unwrap();

        // Send to n2 and record success
        let msgs = leader.heartbeat();
        let (_, req_n2) = msgs.iter().find(|(p, _)| *p == 2).cloned().unwrap();
        let resp_n2 = n2.handle_append_entries(req_n2);

        // Leader processes n2's successful response — now 2/3 have the entry (quorum)
        leader.handle_append_response(2, resp_n2);
        assert_eq!(leader.commit_index(), 1, "entry should be committed after quorum");

        // n3 has NOT yet received the entry (1/3) — commit NOT advanced before n3 responds
        // (this is verified above: commit is 1 after n2 responded, regardless of n3)
        // Also verify n3's log is still empty (entry not replicated yet)
        let msgs2 = leader.heartbeat();
        let (_, req_n3) = msgs2.iter().find(|(p, _)| *p == 3).cloned().unwrap();
        let _resp_n3 = n3.handle_append_entries(req_n3);
        // n3 now has the entry, but the commit was already decided by n2
        assert_eq!(leader.commit_index(), 1);
    }

    #[test]
    fn test_term_increment_on_election() {
        let mut node = RaftNode::new(make_config(1, vec![2, 3]));
        assert_eq!(node.current_term(), 0);

        node.start_election();
        assert_eq!(node.current_term(), 1);

        // Simulate election fails (stepped down), then another election
        node.role = RaftRole::Follower;
        node.start_election();
        assert_eq!(node.current_term(), 2);

        node.role = RaftRole::Follower;
        node.start_election();
        assert_eq!(node.current_term(), 3);
    }

    #[test]
    fn test_snapshot_install() {
        // Leader has a snapshot at index 10; follower calls handle_install_snapshot
        let mut leader = RaftNode::new(make_config(1, vec![2]));
        let mut follower = RaftNode::new(make_config(2, vec![1]));

        // Leader wins election (needs a vote from peer 2)
        leader.start_election();
        let won = leader.handle_vote_response(2, RequestVoteResp { term: 1, vote_granted: true });
        assert!(won, "leader should win election");

        // Append 10 entries
        for i in 0..10u8 {
            leader.propose(vec![i]).unwrap();
        }
        // Leader creates a snapshot at index 10
        leader.create_snapshot(10, b"state_at_10".to_vec());
        assert!(leader.log.snapshot().is_some());
        assert_eq!(leader.log.snapshot().unwrap().last_included_index, 10);

        // Leader sends snapshot to follower
        let req = InstallSnapshotReq {
            term: leader.current_term(),
            leader_id: 1,
            last_included_index: 10,
            last_included_term: 1,
            data: b"state_at_10".to_vec(),
        };
        let resp = follower.handle_install_snapshot(req);
        assert_eq!(resp.term, follower.current_term());

        // Follower's log base should now be at 10
        assert_eq!(follower.log.base_index(), 10);
        assert!(follower.log.snapshot().is_some());
    }

    #[test]
    fn test_leader_sends_snapshot_to_lagging_follower() {
        let mut leader = RaftNode::new(make_config(1, vec![2, 3]));

        // Leader wins election
        leader.start_election();
        leader.handle_vote_response(2, RequestVoteResp { term: 1, vote_granted: true });
        assert!(leader.is_leader());

        // Leader appends 15 entries and creates snapshot at index 10
        for i in 0..15u8 {
            leader.propose(vec![i]).unwrap();
        }
        leader.create_snapshot(10, b"snap".to_vec());

        // Peer 2 has next_index = 1 (lagging far behind the snapshot)
        // maybe_install_snapshot should return Some for peer 2
        let result = leader.maybe_install_snapshot(2);
        assert!(result.is_some(), "leader should want to send snapshot to lagging peer");
        let (peer, req) = result.unwrap();
        assert_eq!(peer, 2);
        assert_eq!(req.last_included_index, 10);

        // Peer 3 also lagging
        let result3 = leader.maybe_install_snapshot(3);
        assert!(result3.is_some());
    }
}
