use crate::log::{LogEntry, LogIndex, Term};
use crate::node::NodeId;

/// Sent by a leader to replicate log entries and serve as a heartbeat.
#[derive(Debug, Clone)]
pub struct AppendEntriesReq {
    pub term: Term,
    pub leader_id: NodeId,
    /// Index of the log entry immediately preceding the new entries.
    pub prev_log_index: LogIndex,
    /// Term of the entry at prev_log_index.
    pub prev_log_term: Term,
    /// New log entries to store (empty for heartbeat).
    pub entries: Vec<LogEntry>,
    /// The leader's commit index.
    pub leader_commit: LogIndex,
}

/// Response to AppendEntries.
#[derive(Debug, Clone)]
pub struct AppendEntriesResp {
    /// Current term; used by leader to update itself.
    pub term: Term,
    /// True if the follower successfully accepted the entries.
    pub success: bool,
    /// The highest log index the follower now has (for fast rollback on failure).
    pub match_index: LogIndex,
}

/// Sent by a candidate to solicit votes during an election.
#[derive(Debug, Clone)]
pub struct RequestVoteReq {
    pub term: Term,
    pub candidate_id: NodeId,
    pub last_log_index: LogIndex,
    pub last_log_term: Term,
}

/// Response to RequestVote.
#[derive(Debug, Clone)]
pub struct RequestVoteResp {
    /// Current term; used by candidate to update itself.
    pub term: Term,
    /// True if the voter granted its vote to the candidate.
    pub vote_granted: bool,
}

/// Sent by a leader to a lagging follower to install a snapshot.
#[derive(Debug, Clone)]
pub struct InstallSnapshotReq {
    pub term: Term,
    pub leader_id: NodeId,
    pub last_included_index: LogIndex,
    pub last_included_term: Term,
    /// Snapshot data bytes.
    pub data: Vec<u8>,
}

/// Response to InstallSnapshot.
#[derive(Debug, Clone)]
pub struct InstallSnapshotResp {
    pub term: Term,
}
