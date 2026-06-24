use crate::buffer_pool::BufferPool;
use crate::error::Result;
use crate::page::{PageId, SLOT_SIZE};
use crate::wal::{WalManager, WalRecord};

/// Physical location of a tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TupleId {
    pub page_id: PageId,
    pub slot_id: u16,
}

/// Free Space Map: tracks free space per page for O(log n) insertion routing.
///
/// Implemented as a binary max-heap tree over u8 values.
/// Each leaf represents one page; its value is `min(255, free_bytes / 32)`.
/// Internal nodes hold the maximum value of their subtree.
pub struct FreeSpaceMap {
    /// 1-indexed binary tree. tree[1] is the root.
    tree: Vec<u8>,
    /// Number of leaf slots (always a power of 2).
    capacity: usize,
    /// PageId of the first page this FSM covers.
    first_page_id: PageId,
}

impl FreeSpaceMap {
    pub fn new(capacity: usize, first_page_id: PageId) -> Self {
        let cap = capacity.next_power_of_two().max(1);
        FreeSpaceMap {
            tree: vec![0; cap * 2], // 1-indexed, size = 2 * cap
            capacity: cap,
            first_page_id,
        }
    }

    /// Record that `page_id` has `free_bytes` bytes of free space.
    pub fn update(&mut self, page_id: PageId, free_bytes: usize) {
        let idx = self.leaf_index(page_id);
        if idx >= self.capacity * 2 {
            return; // page outside FSM range (grow needed, not implemented yet)
        }
        self.tree[idx] = encode_free(free_bytes);
        self.propagate_up(idx);
    }

    /// Find a page that has at least `needed_bytes` of free space.
    ///
    /// Returns `None` if no tracked page has enough space.
    pub fn find_page(&self, needed_bytes: usize) -> Option<PageId> {
        let needed = encode_free(needed_bytes);
        if self.tree[1] < needed {
            return None;
        }
        let mut idx = 1;
        // Descend to a leaf that satisfies the requirement
        while idx < self.capacity {
            let left = 2 * idx;
            let right = 2 * idx + 1;
            if self.tree[left] >= needed {
                idx = left;
            } else {
                idx = right;
            }
        }
        let page_offset = idx - self.capacity;
        Some(self.first_page_id + page_offset as PageId)
    }

    fn leaf_index(&self, page_id: PageId) -> usize {
        self.capacity + (page_id - self.first_page_id) as usize
    }

    fn propagate_up(&mut self, mut idx: usize) {
        idx /= 2;
        while idx >= 1 {
            self.tree[idx] = self.tree[2 * idx].max(self.tree[2 * idx + 1]);
            idx /= 2;
        }
    }
}

fn encode_free(free_bytes: usize) -> u8 {
    ((free_bytes / 32) as u64).min(255) as u8
}

/// A heap file stores a table as an unordered collection of pages.
///
/// Tuple insertion finds a page with sufficient free space via the FSM,
/// writes the tuple, and records a WAL entry. Scanning reads every page
/// in order and returns all used slots.
pub struct HeapFile {
    pub table_id: u32,
    fsm: FreeSpaceMap,
    /// PageIds of all pages belonging to this table.
    pages: Vec<PageId>,
}

impl HeapFile {
    /// Create a new empty heap file.
    pub fn new(table_id: u32) -> Self {
        HeapFile {
            table_id,
            fsm: FreeSpaceMap::new(64, 0),
            pages: Vec::new(),
        }
    }

    /// Insert a raw tuple. Returns the TupleId of the new tuple.
    ///
    /// The caller is responsible for pre-encoding any MVCC header into `tuple_data`.
    pub fn insert_tuple(
        &mut self,
        pool: &mut BufferPool,
        wal: &mut WalManager,
        xid: u64,
        tuple_data: &[u8],
    ) -> Result<TupleId> {
        let needed = SLOT_SIZE + tuple_data.len();
        let page_id = self.find_or_alloc_page(pool, needed)?;

        let frame_id = pool.fetch_page(page_id)?;
        let slot_id = pool.page_mut(frame_id).insert_tuple(tuple_data)?;
        let new_free = pool.page(frame_id).free_space();

        // WAL before marking dirty
        wal.write_record(
            xid,
            WalRecord::HeapInsert {
                table_id: self.table_id,
                page_id,
                slot_id,
                tuple_data: tuple_data.to_vec(),
            },
        )?;

        pool.page_mut(frame_id).set_lsn(wal.flushed_lsn());
        pool.unpin_page(page_id, true)?;
        self.fsm.update(page_id, new_free);

        Ok(TupleId { page_id, slot_id })
    }

    /// Fetch the raw bytes of a tuple by its TupleId.
    pub fn get_tuple(
        &self,
        pool: &mut BufferPool,
        tid: TupleId,
    ) -> Result<Vec<u8>> {
        let frame_id = pool.fetch_page(tid.page_id)?;
        let data = pool.page(frame_id).get_tuple(tid.slot_id)?.to_vec();
        pool.unpin_page(tid.page_id, false)?;
        Ok(data)
    }

    /// Mark a tuple as deleted. Does not reclaim space (vacuum does that).
    pub fn delete_tuple(
        &mut self,
        pool: &mut BufferPool,
        wal: &mut WalManager,
        xid: u64,
        tid: TupleId,
        old_xmax: u64,
    ) -> Result<()> {
        let frame_id = pool.fetch_page(tid.page_id)?;
        pool.page_mut(frame_id).delete_tuple(tid.slot_id)?;

        wal.write_record(
            xid,
            WalRecord::HeapDelete {
                table_id: self.table_id,
                page_id: tid.page_id,
                slot_id: tid.slot_id,
                old_xmax,
            },
        )?;

        pool.page_mut(frame_id).set_lsn(wal.flushed_lsn());
        pool.unpin_page(tid.page_id, true)?;
        Ok(())
    }

    /// Scan all tuples in the heap, returning (TupleId, raw_bytes) for each live slot.
    pub fn scan(&self, pool: &mut BufferPool) -> Result<Vec<(TupleId, Vec<u8>)>> {
        let mut results = Vec::new();

        for &page_id in &self.pages {
            let frame_id = pool.fetch_page(page_id)?;
            let slot_count = pool.page(frame_id).slot_count();

            for slot_id in 0..slot_count {
                if pool.page(frame_id).is_slot_used(slot_id) {
                    let data = pool.page(frame_id).get_tuple(slot_id)?.to_vec();
                    results.push((TupleId { page_id, slot_id }, data));
                }
            }

            pool.unpin_page(page_id, false)?;
        }

        Ok(results)
    }

    /// Return the list of all page IDs belonging to this heap file (for vacuum).
    pub fn page_ids(&self) -> &[PageId] {
        &self.pages
    }

    /// Update the free space map for a page (called by vacuum after compaction).
    pub fn update_fsm(&mut self, page_id: PageId, free_bytes: usize) {
        self.fsm.update(page_id, free_bytes);
    }

    // --- Internal ---

    fn find_or_alloc_page(
        &mut self,
        pool: &mut BufferPool,
        needed: usize,
    ) -> Result<PageId> {
        if let Some(page_id) = self.fsm.find_page(needed) {
            if self.pages.contains(&page_id) {
                return Ok(page_id);
            }
        }

        // No suitable page found — allocate a new one
        let (page_id, _frame_id) = pool.new_page()?;
        pool.unpin_page(page_id, true)?;

        self.pages.push(page_id);
        self.fsm = FreeSpaceMap::new(self.pages.len().next_power_of_two(), self.pages[0]);
        // Update FSM with actual free space of all pages
        for &pid in &self.pages {
            let fid = pool.fetch_page(pid)?;
            let free = pool.page(fid).free_space();
            pool.unpin_page(pid, false)?;
            self.fsm.update(pid, free);
        }

        Ok(page_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::DiskManager;
    use tempfile::tempdir;

    fn make_env() -> (BufferPool, WalManager, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let wal_path = dir.path().join("test.wal");
        let disk = DiskManager::create(&db_path).unwrap();
        let pool = BufferPool::new(16, disk);
        let wal = WalManager::create(&wal_path).unwrap();
        (pool, wal, dir)
    }

    #[test]
    fn test_insert_and_get_single_tuple() {
        let (mut pool, mut wal, _dir) = make_env();
        let mut heap = HeapFile::new(1);

        let tid = heap.insert_tuple(&mut pool, &mut wal, 1, b"hello heap").unwrap();
        wal.flush().unwrap();
        pool.flush_all().unwrap();

        let data = heap.get_tuple(&mut pool, tid).unwrap();
        assert_eq!(data, b"hello heap");
    }

    #[test]
    fn test_insert_many_tuples_spans_pages() {
        let (mut pool, mut wal, _dir) = make_env();
        let mut heap = HeapFile::new(1);

        // Each tuple is 200 bytes. PAGE_SIZE=8192. ~39 tuples per page.
        // Insert 200 tuples -> should span 6+ pages.
        let mut tids = Vec::new();
        for i in 0u8..200 {
            let data = vec![i; 200];
            let tid = heap.insert_tuple(&mut pool, &mut wal, 1, &data).unwrap();
            tids.push((tid, data));
        }

        wal.flush().unwrap();

        // Verify all tuples readable
        for (tid, expected) in &tids {
            let actual = heap.get_tuple(&mut pool, *tid).unwrap();
            assert_eq!(&actual, expected);
        }
    }

    #[test]
    fn test_sequential_scan_returns_all_tuples() {
        let (mut pool, mut wal, _dir) = make_env();
        let mut heap = HeapFile::new(1);

        let mut expected: Vec<Vec<u8>> = Vec::new();
        for i in 0u8..50 {
            let data = vec![i; 64];
            heap.insert_tuple(&mut pool, &mut wal, 1, &data).unwrap();
            expected.push(data);
        }
        wal.flush().unwrap();

        let scanned = heap.scan(&mut pool).unwrap();
        assert_eq!(scanned.len(), 50);
        for (i, (_, data)) in scanned.iter().enumerate() {
            assert_eq!(data, &expected[i]);
        }
    }

    #[test]
    fn test_delete_removes_from_scan() {
        let (mut pool, mut wal, _dir) = make_env();
        let mut heap = HeapFile::new(1);

        let tid1 = heap.insert_tuple(&mut pool, &mut wal, 1, b"keep").unwrap();
        let tid2 = heap.insert_tuple(&mut pool, &mut wal, 1, b"delete me").unwrap();
        heap.delete_tuple(&mut pool, &mut wal, 1, tid2, 0).unwrap();
        wal.flush().unwrap();

        let scanned = heap.scan(&mut pool).unwrap();
        assert_eq!(scanned.len(), 1);
        assert_eq!(scanned[0].0, tid1);
        assert_eq!(scanned[0].1, b"keep");
    }

    #[test]
    fn test_wal_records_written_for_inserts() {
        let (mut pool, mut wal, _dir) = make_env();
        let mut heap = HeapFile::new(1);

        for _ in 0..10 {
            heap.insert_tuple(&mut pool, &mut wal, 42, b"wal check").unwrap();
        }
        wal.flush().unwrap();

        let mut reader = wal.read_from(0).unwrap();
        let mut count = 0;
        while let Some(res) = reader.next_record() {
            let rec = res.unwrap();
            if matches!(rec.record, WalRecord::HeapInsert { .. }) {
                count += 1;
            }
        }
        assert_eq!(count, 10);
    }

    #[test]
    fn test_fsm_routes_to_existing_page() {
        let (mut pool, mut wal, _dir) = make_env();
        let mut heap = HeapFile::new(1);

        // Insert one small tuple — takes up one page
        heap.insert_tuple(&mut pool, &mut wal, 1, b"small").unwrap();
        let pages_before = heap.pages.len();

        // Insert another small tuple — should reuse the same page
        heap.insert_tuple(&mut pool, &mut wal, 1, b"also small").unwrap();
        assert_eq!(heap.pages.len(), pages_before, "second insert should reuse page");
    }
}
