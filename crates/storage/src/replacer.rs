use std::collections::{BTreeMap, HashMap, HashSet};

pub type FrameId = usize;

/// LRU page-frame eviction policy.
///
/// Tracks which frames are evictable (pin_count == 0) and which frame was
/// least recently accessed among the evictable set. The buffer pool calls
/// `record_access` when a frame is pinned and `set_evictable` when it is
/// unpinned.
pub struct LruReplacer {
    /// Frames currently eligible for eviction.
    evictable: HashSet<FrameId>,
    /// Monotonic access timestamp -> frame_id (ascending = oldest first).
    access_order: BTreeMap<u64, FrameId>,
    /// frame_id -> its current timestamp in access_order.
    frame_timestamp: HashMap<FrameId, u64>,
    /// Monotonically increasing clock for access timestamps.
    clock: u64,
}

impl LruReplacer {
    pub fn new(_capacity: usize) -> Self {
        LruReplacer {
            evictable: HashSet::new(),
            access_order: BTreeMap::new(),
            frame_timestamp: HashMap::new(),
            clock: 0,
        }
    }

    /// Record that `frame_id` was accessed. Updates its position in the LRU order.
    pub fn record_access(&mut self, frame_id: FrameId) {
        // Remove old timestamp entry if present
        if let Some(&old_ts) = self.frame_timestamp.get(&frame_id) {
            self.access_order.remove(&old_ts);
        }
        self.clock += 1;
        let ts = self.clock;
        self.access_order.insert(ts, frame_id);
        self.frame_timestamp.insert(frame_id, ts);
    }

    /// Mark whether `frame_id` may be evicted.
    ///
    /// Call with `evictable = true` when pin_count reaches 0.
    /// Call with `evictable = false` when pin_count goes above 0.
    pub fn set_evictable(&mut self, frame_id: FrameId, evictable: bool) {
        if evictable {
            self.evictable.insert(frame_id);
        } else {
            self.evictable.remove(&frame_id);
        }
    }

    /// Evict the least recently accessed evictable frame.
    ///
    /// Returns `None` if no frames are evictable.
    pub fn evict(&mut self) -> Option<FrameId> {
        // BTreeMap iterates in ascending key order (oldest timestamp first)
        let (&ts, &frame_id) = self
            .access_order
            .iter()
            .find(|(_, fid)| self.evictable.contains(fid))?;

        self.access_order.remove(&ts);
        self.frame_timestamp.remove(&frame_id);
        self.evictable.remove(&frame_id);
        Some(frame_id)
    }

    /// Remove all tracking for `frame_id` (e.g., when the frame is deleted).
    pub fn remove(&mut self, frame_id: FrameId) {
        if let Some(&ts) = self.frame_timestamp.get(&frame_id) {
            self.access_order.remove(&ts);
        }
        self.frame_timestamp.remove(&frame_id);
        self.evictable.remove(&frame_id);
    }

    /// Number of frames currently eligible for eviction.
    pub fn evictable_count(&self) -> usize {
        self.evictable.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_eviction_order() {
        let mut r = LruReplacer::new(5);
        // Access A=0, B=1, C=2 in order
        r.record_access(0);
        r.record_access(1);
        r.record_access(2);
        r.set_evictable(0, true);
        r.set_evictable(1, true);
        r.set_evictable(2, true);

        // LRU eviction: 0 was accessed first, should go first
        assert_eq!(r.evict(), Some(0));
        assert_eq!(r.evict(), Some(1));
        assert_eq!(r.evict(), Some(2));
        assert_eq!(r.evict(), None);
    }

    #[test]
    fn test_pinned_frames_not_evicted() {
        let mut r = LruReplacer::new(5);
        r.record_access(0);
        r.record_access(1);
        r.record_access(2);
        // Only mark 0 and 2 evictable; 1 is pinned
        r.set_evictable(0, true);
        r.set_evictable(2, true);

        assert_eq!(r.evict(), Some(0));
        assert_eq!(r.evict(), Some(2));
        assert_eq!(r.evict(), None); // 1 is pinned, cannot evict
        assert_eq!(r.evictable_count(), 0);
    }

    #[test]
    fn test_re_access_updates_lru_order() {
        let mut r = LruReplacer::new(5);
        r.record_access(0); // oldest
        r.record_access(1);
        r.record_access(2);
        // Re-access 0: now 0 is most recently used
        r.record_access(0);
        r.set_evictable(0, true);
        r.set_evictable(1, true);
        r.set_evictable(2, true);

        // 1 was accessed least recently now
        assert_eq!(r.evict(), Some(1));
        assert_eq!(r.evict(), Some(2));
        assert_eq!(r.evict(), Some(0)); // 0 was re-accessed, evicted last
    }

    #[test]
    fn test_set_evictable_false_prevents_eviction() {
        let mut r = LruReplacer::new(3);
        r.record_access(0);
        r.set_evictable(0, true);
        r.set_evictable(0, false); // pin it back
        assert_eq!(r.evict(), None);
    }

    #[test]
    fn test_remove_clears_frame() {
        let mut r = LruReplacer::new(3);
        r.record_access(0);
        r.set_evictable(0, true);
        r.remove(0);
        assert_eq!(r.evictable_count(), 0);
        assert_eq!(r.evict(), None);
    }

    #[test]
    fn test_evictable_count() {
        let mut r = LruReplacer::new(5);
        for i in 0..5 {
            r.record_access(i);
            r.set_evictable(i, true);
        }
        assert_eq!(r.evictable_count(), 5);
        r.set_evictable(2, false);
        assert_eq!(r.evictable_count(), 4);
        r.evict();
        assert_eq!(r.evictable_count(), 3);
    }
}
