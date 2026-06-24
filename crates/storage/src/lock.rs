/// Two-phase locking (2PL) lock manager.
use crate::heap::TupleId;
use crate::mvcc::Xid;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Condvar, Mutex};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LockTarget {
    Table(u32),
    Tuple(u32, TupleId),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

pub struct LockRequest {
    pub xid: Xid,
    pub target: LockTarget,
    pub mode: LockMode,
}

struct LockEntry {
    holders: Vec<(Xid, LockMode)>,
    waiters: VecDeque<(Xid, LockMode)>,
}

impl LockEntry {
    fn new() -> Self {
        LockEntry {
            holders: Vec::new(),
            waiters: VecDeque::new(),
        }
    }

    /// Returns true if the given mode is compatible with all current holders.
    fn all_compatible(&self, mode: &LockMode) -> bool {
        self.holders.iter().all(|(_, held)| is_compatible(held, mode))
    }

    /// Returns true if xid already holds a lock at least as strong as `mode`.
    fn already_holds(&self, xid: Xid, mode: &LockMode) -> bool {
        self.holders.iter().any(|(hxid, hmode)| {
            *hxid == xid && (hmode == mode || *hmode == LockMode::Exclusive)
        })
    }
}

pub struct LockManager {
    locks: Mutex<HashMap<LockTarget, LockEntry>>,
    condvar: Condvar,
}

impl LockManager {
    pub fn new() -> Self {
        LockManager {
            locks: Mutex::new(HashMap::new()),
            condvar: Condvar::new(),
        }
    }

    /// Acquire a lock. Returns Ok(()) when granted, Err("deadlock") on cycle.
    pub fn acquire(&self, req: LockRequest) -> Result<(), String> {
        let mut map = self.locks.lock().unwrap();

        // Fast path: already held
        if let Some(entry) = map.get(&req.target) {
            if entry.already_holds(req.xid, &req.mode) {
                return Ok(());
            }
        }

        loop {
            let entry = map.entry(req.target.clone()).or_insert_with(LockEntry::new);

            if entry.all_compatible(&req.mode) && entry.waiters.is_empty() {
                // Grant immediately
                entry.holders.push((req.xid, req.mode));
                return Ok(());
            }

            // Check for deadlock before waiting
            if detect_deadlock(req.xid, &map) {
                return Err("deadlock".to_string());
            }

            // Queue as waiter
            let entry = map.entry(req.target.clone()).or_insert_with(LockEntry::new);
            // Add waiter only if not already in queue
            if !entry.waiters.iter().any(|(wxid, _)| *wxid == req.xid) {
                entry.waiters.push_back((req.xid, req.mode.clone()));
            }

            // Wait for a signal
            map = self.condvar.wait(map).unwrap();

            // After waking up, remove self from waiters and try again
            if let Some(entry) = map.get_mut(&req.target) {
                entry.waiters.retain(|(wxid, _)| *wxid != req.xid);
            }
        }
    }

    /// Release all locks held by xid.
    pub fn release_all(&self, xid: Xid) {
        let mut map = self.locks.lock().unwrap();
        for entry in map.values_mut() {
            entry.holders.retain(|(hxid, _)| *hxid != xid);
        }
        // Remove empty entries
        map.retain(|_, entry| !entry.holders.is_empty() || !entry.waiters.is_empty());
        self.condvar.notify_all();
    }

    /// Return all XIDs that hold locks on target.
    pub fn holders(&self, target: &LockTarget) -> Vec<Xid> {
        let map = self.locks.lock().unwrap();
        map.get(target)
            .map(|e| e.holders.iter().map(|(xid, _)| *xid).collect())
            .unwrap_or_default()
    }
}

impl Default for LockManager {
    fn default() -> Self {
        Self::new()
    }
}

fn is_compatible(held: &LockMode, requested: &LockMode) -> bool {
    matches!((held, requested), (LockMode::Shared, LockMode::Shared))
}

/// Simple deadlock detection: look for a cycle in the waits-for graph.
/// Returns true if waiting_xid is part of a deadlock cycle.
fn detect_deadlock(waiting_xid: Xid, locks: &HashMap<LockTarget, LockEntry>) -> bool {
    // Build a waits-for graph: if xid A is waiting and xid B holds, A waits for B.
    // Check if any holder of the targets we wait on, in turn waits for us.
    let mut visited: HashSet<Xid> = HashSet::new();
    let mut stack: Vec<Xid> = Vec::new();
    stack.push(waiting_xid);

    while let Some(xid) = stack.pop() {
        if !visited.insert(xid) {
            continue;
        }
        // Find targets that xid is waiting on
        for entry in locks.values() {
            let is_waiting = entry.waiters.iter().any(|(wxid, _)| *wxid == xid);
            if is_waiting {
                // xid is waiting; the holders block it
                for (hxid, _) in &entry.holders {
                    if *hxid == waiting_xid && xid != waiting_xid {
                        // Found a cycle back to waiting_xid
                        return true;
                    }
                    stack.push(*hxid);
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn tid(p: u64, s: u16) -> TupleId {
        TupleId { page_id: p, slot_id: s }
    }

    #[test]
    fn test_shared_locks_compatible() {
        let lm = LockManager::new();
        let target = LockTarget::Table(1);

        lm.acquire(LockRequest { xid: 1, target: target.clone(), mode: LockMode::Shared }).unwrap();
        lm.acquire(LockRequest { xid: 2, target: target.clone(), mode: LockMode::Shared }).unwrap();

        let holders = lm.holders(&target);
        assert!(holders.contains(&1));
        assert!(holders.contains(&2));
    }

    #[test]
    fn test_exclusive_blocks_shared() {
        let lm = Arc::new(LockManager::new());
        let target = LockTarget::Tuple(1, tid(0, 0));

        // A holds Exclusive
        lm.acquire(LockRequest { xid: 10, target: target.clone(), mode: LockMode::Exclusive }).unwrap();

        let lm2 = Arc::clone(&lm);
        let tgt2 = target.clone();
        let handle = std::thread::spawn(move || {
            // B requests Shared — will block until A releases
            lm2.acquire(LockRequest { xid: 20, target: tgt2, mode: LockMode::Shared }).unwrap();
        });

        // Give B a moment to block
        std::thread::sleep(std::time::Duration::from_millis(50));

        // A releases
        lm.release_all(10);

        // B should now complete
        handle.join().unwrap();
    }

    #[test]
    fn test_exclusive_blocks_exclusive() {
        let lm = Arc::new(LockManager::new());
        let target = LockTarget::Table(99);

        // A holds Exclusive
        lm.acquire(LockRequest { xid: 1, target: target.clone(), mode: LockMode::Exclusive }).unwrap();

        let lm2 = Arc::clone(&lm);
        let tgt2 = target.clone();
        let handle = std::thread::spawn(move || {
            lm2.acquire(LockRequest { xid: 2, target: tgt2, mode: LockMode::Exclusive }).unwrap();
        });

        std::thread::sleep(std::time::Duration::from_millis(50));
        lm.release_all(1);
        handle.join().unwrap();
    }

    #[test]
    fn test_release_unblocks_waiter() {
        let lm = Arc::new(LockManager::new());
        let target = LockTarget::Table(7);

        lm.acquire(LockRequest { xid: 5, target: target.clone(), mode: LockMode::Exclusive }).unwrap();

        let lm2 = Arc::clone(&lm);
        let tgt2 = target.clone();
        let handle = std::thread::spawn(move || {
            lm2.acquire(LockRequest { xid: 6, target: tgt2.clone(), mode: LockMode::Exclusive }).unwrap();
            let holders = lm2.holders(&tgt2);
            assert!(holders.contains(&6));
        });

        std::thread::sleep(std::time::Duration::from_millis(50));
        // Verify B is not yet holding
        assert!(!lm.holders(&target).contains(&6));

        lm.release_all(5);
        handle.join().unwrap();
    }

    #[test]
    fn test_no_deadlock_two_compatible() {
        let lm = LockManager::new();
        let target = LockTarget::Table(42);

        lm.acquire(LockRequest { xid: 100, target: target.clone(), mode: LockMode::Shared }).unwrap();
        // Second shared lock is immediately compatible — no deadlock
        let result = lm.acquire(LockRequest { xid: 200, target: target.clone(), mode: LockMode::Shared });
        assert!(result.is_ok());

        let holders = lm.holders(&target);
        assert!(holders.contains(&100));
        assert!(holders.contains(&200));
    }
}
