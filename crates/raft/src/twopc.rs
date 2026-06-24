/// Two-phase commit coordinator for cross-shard transactions.
use crate::cluster::RaftCluster;
use crate::node::NodeId;
use crate::shard::ShardId;
use crate::state_machine::StateMachine;
use std::collections::HashMap;

pub type TxnId = u64;

#[derive(Debug, Clone, PartialEq)]
pub enum TxnState {
    Preparing,
    Committed,
    Aborted,
}

#[derive(Debug, Clone)]
pub struct Participant {
    pub shard_id: ShardId,
    pub node_id: NodeId,
}

#[derive(Debug, Clone)]
pub struct TwoPhaseCoordinator {
    pub txn_id: TxnId,
    pub state: TxnState,
    pub participants: Vec<Participant>,
    prepare_votes: HashMap<ShardId, bool>,
    /// Acknowledgments received from participants during phase 2.
    finalize_acks: HashMap<ShardId, bool>,
}

#[derive(Debug, Clone)]
pub enum CoordinatorMsg {
    Prepare { txn_id: TxnId },
    Commit  { txn_id: TxnId },
    Abort   { txn_id: TxnId },
}

#[derive(Debug, Clone)]
pub enum ParticipantMsg {
    PrepareOk    { txn_id: TxnId, shard_id: ShardId },
    PrepareAbort { txn_id: TxnId, shard_id: ShardId },
    CommitAck    { txn_id: TxnId, shard_id: ShardId },
    AbortAck     { txn_id: TxnId, shard_id: ShardId },
}

impl TwoPhaseCoordinator {
    pub fn new(txn_id: TxnId, participants: Vec<Participant>) -> Self {
        TwoPhaseCoordinator {
            txn_id,
            state: TxnState::Preparing,
            participants,
            prepare_votes: HashMap::new(),
            finalize_acks: HashMap::new(),
        }
    }

    /// Begin phase 1: returns Prepare messages to send to all participants.
    pub fn begin_prepare(&self) -> Vec<(Participant, CoordinatorMsg)> {
        self.participants.iter().map(|p| {
            (p.clone(), CoordinatorMsg::Prepare { txn_id: self.txn_id })
        }).collect()
    }

    /// Record a vote from a participant.
    /// Returns:
    ///   Some(true)  = all votes received, decision is Commit
    ///   Some(false) = all votes received, decision is Abort (at least one No)
    ///   None        = still waiting for more votes
    pub fn record_vote(&mut self, msg: ParticipantMsg) -> Option<bool> {
        let (shard_id, vote) = match &msg {
            ParticipantMsg::PrepareOk    { shard_id, .. } => (*shard_id, true),
            ParticipantMsg::PrepareAbort { shard_id, .. } => (*shard_id, false),
            _ => return None, // not a phase-1 message
        };

        self.prepare_votes.insert(shard_id, vote);

        // If any vote is false, we can immediately decide Abort
        if !vote {
            return Some(false);
        }

        // Check if all participants have voted
        let all_voted = self.participants.iter().all(|p| self.prepare_votes.contains_key(&p.shard_id));
        if all_voted {
            // All votes were true (otherwise we'd have returned Some(false) earlier)
            Some(true)
        } else {
            None
        }
    }

    /// Begin phase 2: returns Commit or Abort messages based on decision.
    /// Panics if called before record_vote returns Some.
    pub fn begin_finalize(&mut self) -> Vec<(Participant, CoordinatorMsg)> {
        // Determine decision based on votes
        let commit = self.participants.iter().all(|p| {
            self.prepare_votes.get(&p.shard_id).copied().unwrap_or(false)
        });

        self.state = if commit { TxnState::Committed } else { TxnState::Aborted };

        self.participants.iter().map(|p| {
            let msg = if commit {
                CoordinatorMsg::Commit { txn_id: self.txn_id }
            } else {
                CoordinatorMsg::Abort { txn_id: self.txn_id }
            };
            (p.clone(), msg)
        }).collect()
    }

    /// Record a finalization acknowledgment.
    pub fn record_ack(&mut self, msg: ParticipantMsg) {
        let shard_id = match &msg {
            ParticipantMsg::CommitAck { shard_id, .. } => *shard_id,
            ParticipantMsg::AbortAck  { shard_id, .. } => *shard_id,
            _ => return, // not a phase-2 message
        };
        self.finalize_acks.insert(shard_id, true);
    }

    /// Returns true if all participants have sent their phase-2 acknowledgment.
    pub fn is_complete(&self) -> bool {
        self.participants.iter().all(|p| self.finalize_acks.contains_key(&p.shard_id))
    }

    pub fn state(&self) -> &TxnState {
        &self.state
    }
}

// ---------------------------------------------------------------------------
// TwoPhaseRaftCoordinator
// ---------------------------------------------------------------------------

/// Coordinates a distributed 2PC transaction through a Raft-replicated log.
pub struct TwoPhaseRaftCoordinator<S: StateMachine + Send + 'static> {
    cluster: std::sync::Arc<RaftCluster<S>>,
    txn_counter: std::sync::atomic::AtomicU64,
}

impl<S: StateMachine + Send + 'static> TwoPhaseRaftCoordinator<S> {
    pub fn new(cluster: std::sync::Arc<RaftCluster<S>>) -> Self {
        TwoPhaseRaftCoordinator {
            cluster,
            txn_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Execute a transactional command through raft-coordinated 2PC.
    /// Returns Ok(true) if committed, Ok(false) if aborted.
    pub fn execute_transaction(&self, cmd: String) -> Result<bool, String> {
        let txn_id = self
            .txn_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.cluster.two_phase_propose(txn_id, cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_participants(n: usize) -> Vec<Participant> {
        (1..=n as u64).map(|i| Participant { shard_id: i, node_id: i }).collect()
    }

    #[test]
    fn test_2pc_commit_path() {
        let participants = make_participants(3);
        let mut coord = TwoPhaseCoordinator::new(1, participants);

        // Phase 1: send prepares
        let msgs = coord.begin_prepare();
        assert_eq!(msgs.len(), 3);

        // All participants vote PrepareOk
        let result1 = coord.record_vote(ParticipantMsg::PrepareOk { txn_id: 1, shard_id: 1 });
        assert!(result1.is_none(), "not all votes in yet");

        let result2 = coord.record_vote(ParticipantMsg::PrepareOk { txn_id: 1, shard_id: 2 });
        assert!(result2.is_none(), "not all votes in yet");

        let result3 = coord.record_vote(ParticipantMsg::PrepareOk { txn_id: 1, shard_id: 3 });
        assert_eq!(result3, Some(true), "all voted yes, should commit");

        // Phase 2: begin finalize
        let finalize_msgs = coord.begin_finalize();
        assert_eq!(finalize_msgs.len(), 3);
        assert!(matches!(finalize_msgs[0].1, CoordinatorMsg::Commit { .. }));
        assert_eq!(coord.state(), &TxnState::Committed);
    }

    #[test]
    fn test_2pc_abort_on_any_no() {
        let participants = make_participants(3);
        let mut coord = TwoPhaseCoordinator::new(2, participants);

        coord.begin_prepare();

        // Two yes votes
        coord.record_vote(ParticipantMsg::PrepareOk { txn_id: 2, shard_id: 1 });
        coord.record_vote(ParticipantMsg::PrepareOk { txn_id: 2, shard_id: 2 });

        // One no vote — should immediately decide abort
        let result = coord.record_vote(ParticipantMsg::PrepareAbort { txn_id: 2, shard_id: 3 });
        assert_eq!(result, Some(false), "abort vote should trigger abort decision");

        // Phase 2: should produce Abort messages
        let finalize_msgs = coord.begin_finalize();
        assert!(matches!(finalize_msgs[0].1, CoordinatorMsg::Abort { .. }));
        assert_eq!(coord.state(), &TxnState::Aborted);
    }

    // Compile-time check: TwoPhaseRaftCoordinator is constructable with KvStateMachine.
    #[allow(dead_code)]
    fn _compile_check_two_phase_coordinator() {
        let _ = std::mem::size_of::<TwoPhaseRaftCoordinator<crate::state_machine::KvStateMachine>>();
    }

    #[test]
    fn test_2pc_completion() {
        let participants = make_participants(2);
        let mut coord = TwoPhaseCoordinator::new(3, participants);

        coord.begin_prepare();
        coord.record_vote(ParticipantMsg::PrepareOk { txn_id: 3, shard_id: 1 });
        coord.record_vote(ParticipantMsg::PrepareOk { txn_id: 3, shard_id: 2 });
        coord.begin_finalize();

        // No acks yet
        assert!(!coord.is_complete());

        // Record acks from both participants
        coord.record_ack(ParticipantMsg::CommitAck { txn_id: 3, shard_id: 1 });
        assert!(!coord.is_complete());

        coord.record_ack(ParticipantMsg::CommitAck { txn_id: 3, shard_id: 2 });
        assert!(coord.is_complete());
    }
}
