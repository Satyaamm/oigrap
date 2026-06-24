use crate::log::LogEntry;
use std::collections::HashMap;

/// Trait for a deterministic state machine driven by RAFT.
pub trait StateMachine: Send {
    /// Apply a committed log entry to the state machine.
    fn apply(&mut self, entry: &LogEntry);
    /// Serialize the entire state machine for snapshotting.
    fn snapshot(&self) -> Vec<u8>;
    /// Restore state from a previously taken snapshot.
    fn restore(&mut self, snapshot: Vec<u8>);
}

/// A simple in-memory key-value state machine for testing.
/// Commands are encoded as "SET key=value" or "DEL key".
pub struct KvStateMachine {
    pub data: HashMap<String, String>,
}

impl KvStateMachine {
    pub fn new() -> Self {
        KvStateMachine { data: HashMap::new() }
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.data.get(key)
    }
}

impl Default for KvStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine for KvStateMachine {
    fn apply(&mut self, entry: &LogEntry) {
        let cmd = match std::str::from_utf8(&entry.data) {
            Ok(s) => s,
            Err(_) => return,
        };
        if let Some(rest) = cmd.strip_prefix("SET ") {
            if let Some(eq) = rest.find('=') {
                let key = rest[..eq].to_string();
                let val = rest[eq + 1..].to_string();
                self.data.insert(key, val);
            }
        } else if let Some(key) = cmd.strip_prefix("DEL ") {
            self.data.remove(key);
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        // Simple serialization: "key=value\n" per entry
        let mut out = String::new();
        let mut pairs: Vec<(&String, &String)> = self.data.iter().collect();
        pairs.sort_by_key(|(k, _)| k.as_str());
        for (k, v) in pairs {
            out.push_str(k);
            out.push('=');
            out.push_str(v);
            out.push('\n');
        }
        out.into_bytes()
    }

    fn restore(&mut self, snapshot: Vec<u8>) {
        self.data.clear();
        let s = match std::str::from_utf8(&snapshot) {
            Ok(s) => s,
            Err(_) => return,
        };
        for line in s.lines() {
            if let Some(eq) = line.find('=') {
                self.data.insert(line[..eq].to_string(), line[eq + 1..].to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kv_apply_set_del() {
        let mut sm = KvStateMachine::new();
        let entry_set = LogEntry { term: 1, index: 1, data: b"SET foo=bar".to_vec() };
        sm.apply(&entry_set);
        assert_eq!(sm.get("foo"), Some(&"bar".to_string()));

        let entry_del = LogEntry { term: 1, index: 2, data: b"DEL foo".to_vec() };
        sm.apply(&entry_del);
        assert!(sm.get("foo").is_none());
    }

    #[test]
    fn test_kv_snapshot_restore() {
        let mut sm = KvStateMachine::new();
        let e = LogEntry { term: 1, index: 1, data: b"SET x=42".to_vec() };
        sm.apply(&e);
        let snap = sm.snapshot();

        let mut sm2 = KvStateMachine::new();
        sm2.restore(snap);
        assert_eq!(sm2.get("x"), Some(&"42".to_string()));
    }
}
