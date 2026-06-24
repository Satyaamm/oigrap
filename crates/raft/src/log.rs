pub type LogIndex = u64;
pub type Term = u64;

/// A single entry in the RAFT log.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub term: Term,
    pub index: LogIndex,
    /// Serialized command payload.
    pub data: Vec<u8>,
}

/// A snapshot of the state machine up to a certain log index.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub last_included_index: LogIndex,
    pub last_included_term: Term,
    /// State machine snapshot bytes.
    pub data: Vec<u8>,
}

/// The RAFT replicated log.
pub struct RaftLog {
    /// In-memory entries. After compaction, entries before the snapshot base are discarded.
    /// The first entry is either the sentinel (index 0, term 0) or a synthetic entry
    /// representing the snapshot base.
    entries: Vec<LogEntry>,
    commit_index: LogIndex,
    last_applied: LogIndex,
    /// Current snapshot, if any.
    snapshot: Option<Snapshot>,
    /// The log index at which the current in-memory entries start.
    /// 0 means no compaction; entries[i] has index = base_index + i (for i > 0 when base=0,
    /// or entries[0] is the snapshot base sentinel when base > 0).
    base_index: LogIndex,
}

impl RaftLog {
    pub fn new() -> Self {
        // Sentinel entry at index 0
        let sentinel = LogEntry { term: 0, index: 0, data: vec![] };
        RaftLog {
            entries: vec![sentinel],
            commit_index: 0,
            last_applied: 0,
            snapshot: None,
            base_index: 0,
        }
    }

    /// Append a new entry at the next index. Returns the assigned index.
    pub fn append(&mut self, term: Term, data: Vec<u8>) -> LogIndex {
        let index = self.base_index + self.entries.len() as LogIndex;
        self.entries.push(LogEntry { term, index, data });
        index
    }

    /// Get the entry at `index`, or None if out of range or compacted away.
    pub fn get(&self, index: LogIndex) -> Option<&LogEntry> {
        if index < self.base_index {
            return None; // compacted away
        }
        let pos = (index - self.base_index) as usize;
        self.entries.get(pos)
    }

    /// Index of the last entry.
    pub fn last_index(&self) -> LogIndex {
        self.base_index + (self.entries.len() as LogIndex).saturating_sub(1)
    }

    /// Term of the last entry.
    pub fn last_term(&self) -> Term {
        self.entries.last().map(|e| e.term).unwrap_or(0)
    }

    /// Truncate all entries after `index` (exclusive), discarding them.
    /// Used during log reconciliation when a leader overrides a follower's conflicting entries.
    pub fn truncate_after(&mut self, index: LogIndex) {
        if index < self.base_index {
            // Everything has been compacted; can't truncate before base
            return;
        }
        let truncate_at = (index - self.base_index) as usize + 1;
        if truncate_at < self.entries.len() {
            self.entries.truncate(truncate_at);
            // Adjust commit_index if it was pointing past the truncation point
            if self.commit_index > index {
                self.commit_index = index;
            }
        }
    }

    /// Current commit index.
    pub fn commit_index(&self) -> LogIndex {
        self.commit_index
    }

    /// Advance the commit index up to `index` (monotonically).
    pub fn advance_commit(&mut self, index: LogIndex) {
        if index > self.commit_index {
            self.commit_index = index.min(self.last_index());
        }
    }

    /// Return a slice of entries starting from `index` (inclusive).
    /// Returns empty slice if the index is before the snapshot base.
    pub fn entries_from(&self, index: LogIndex) -> &[LogEntry] {
        if index < self.base_index {
            // Entire range compacted; return nothing (caller should use snapshot)
            return &[];
        }
        let start = (index - self.base_index) as usize;
        if start >= self.entries.len() {
            return &[];
        }
        &self.entries[start..]
    }

    /// Last applied index (entries delivered to the state machine).
    pub fn last_applied(&self) -> LogIndex {
        self.last_applied
    }

    /// Advance last_applied up to commit_index, returning newly applicable entries.
    pub fn take_applicable(&mut self) -> Vec<LogEntry> {
        let mut out = Vec::new();
        while self.last_applied < self.commit_index {
            self.last_applied += 1;
            if let Some(e) = self.get(self.last_applied) {
                out.push(e.clone());
            }
        }
        out
    }

    /// Compact the log: discard all entries up to and including `snapshot.last_included_index`.
    /// Store the snapshot for future InstallSnapshot RPCs.
    pub fn compact(&mut self, snapshot: Snapshot) {
        let snap_index = snapshot.last_included_index;
        let snap_term = snapshot.last_included_term;

        if snap_index <= self.base_index {
            // Already compacted past this point
            return;
        }

        // How many entries to discard
        let new_base = snap_index;
        if new_base >= self.base_index + self.entries.len() as LogIndex {
            // Snapshot covers everything — reset to just the sentinel
            self.entries = vec![LogEntry { term: snap_term, index: snap_index, data: vec![] }];
        } else {
            let discard_count = (new_base - self.base_index) as usize;
            self.entries.drain(0..discard_count);
            // entries[0] is now the entry AT snap_index
            // Replace it with a synthetic sentinel for the snapshot base
            if let Some(first) = self.entries.first_mut() {
                first.index = snap_index;
                first.term = snap_term;
                first.data = vec![];
            }
        }
        self.base_index = new_base;

        // Advance last_applied if needed
        if self.last_applied < snap_index {
            self.last_applied = snap_index;
        }
        if self.commit_index < snap_index {
            self.commit_index = snap_index;
        }

        self.snapshot = Some(snapshot);
    }

    /// Return the current snapshot if one exists.
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    /// True if log[index] has been compacted away (is before snapshot base).
    pub fn is_compacted(&self, index: LogIndex) -> bool {
        index < self.base_index
    }

    /// The base index (first valid index in the in-memory entries).
    pub fn base_index(&self) -> LogIndex {
        self.base_index
    }
}

impl Default for RaftLog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_append_and_get() {
        let mut log = RaftLog::new();
        assert_eq!(log.last_index(), 0);

        let idx = log.append(1, b"cmd1".to_vec());
        assert_eq!(idx, 1);
        assert_eq!(log.last_index(), 1);
        assert_eq!(log.last_term(), 1);

        let entry = log.get(1).unwrap();
        assert_eq!(entry.term, 1);
        assert_eq!(entry.data, b"cmd1");
    }

    #[test]
    fn test_log_truncate() {
        let mut log = RaftLog::new();
        log.append(1, b"a".to_vec());
        log.append(1, b"b".to_vec());
        log.append(2, b"c".to_vec());
        assert_eq!(log.last_index(), 3);

        log.truncate_after(1);
        assert_eq!(log.last_index(), 1);
        assert!(log.get(2).is_none());
    }

    #[test]
    fn test_log_entries_from() {
        let mut log = RaftLog::new();
        log.append(1, b"x".to_vec());
        log.append(1, b"y".to_vec());
        let entries = log.entries_from(1);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_commit_advance() {
        let mut log = RaftLog::new();
        log.append(1, b"a".to_vec());
        log.append(1, b"b".to_vec());
        log.advance_commit(2);
        assert_eq!(log.commit_index(), 2);

        // Commit index does not go backwards
        log.advance_commit(1);
        assert_eq!(log.commit_index(), 2);
    }

    #[test]
    fn test_log_compaction() {
        let mut log = RaftLog::new();
        // Append 20 entries
        for i in 1..=20u8 {
            log.append(1, vec![i]);
        }
        assert_eq!(log.last_index(), 20);

        // Compact at index 10
        let snap = Snapshot { last_included_index: 10, last_included_term: 1, data: b"snap".to_vec() };
        log.compact(snap);

        // Entries 1-9 should be gone (compacted)
        for i in 1..10 {
            assert!(log.get(i).is_none(), "entry {} should be compacted", i);
            assert!(log.is_compacted(i), "entry {} should be marked compacted", i);
        }

        // Entry 10 (snapshot base sentinel) should be accessible
        assert!(log.get(10).is_some());

        // Entries 11-20 should still be present
        for i in 11..=20 {
            let e = log.get(i);
            assert!(e.is_some(), "entry {} should still be present", i);
            assert_eq!(e.unwrap().data, vec![(i) as u8]);
        }

        // Snapshot should be stored
        assert!(log.snapshot().is_some());
        assert_eq!(log.snapshot().unwrap().last_included_index, 10);
    }
}
