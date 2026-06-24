use crate::disk::DiskManager;
use crate::error::{Result, StorageError};
use crate::page::{Page, PageId, PAGE_SIZE};
use crate::replacer::{FrameId, LruReplacer};
use std::collections::{HashMap, VecDeque};

struct Frame {
    page: Page,
    page_id: Option<PageId>,
    pin_count: u32,
    is_dirty: bool,
}

impl Frame {
    fn empty(frame_id: FrameId) -> Self {
        Frame {
            page: Page::new(frame_id as u64), // placeholder page_id
            page_id: None,
            pin_count: 0,
            is_dirty: false,
        }
    }
}

/// Fixed-size buffer pool. All database I/O goes through here.
///
/// Pages must be fetched before use and unpinned when done. An unpinned dirty
/// page will eventually be flushed to disk by the eviction path or an explicit
/// `flush_page` / `flush_all` call.
pub struct BufferPool {
    frames: Vec<Frame>,
    /// page_id -> frame_id for pages currently in the pool
    page_table: HashMap<PageId, FrameId>,
    /// Frames that have never held a page (completely unused)
    free_list: VecDeque<FrameId>,
    replacer: LruReplacer,
    disk: DiskManager,
}

impl BufferPool {
    /// Create a buffer pool with `pool_size` frames backed by `disk`.
    pub fn new(pool_size: usize, disk: DiskManager) -> Self {
        let frames: Vec<Frame> = (0..pool_size).map(Frame::empty).collect();
        let free_list: VecDeque<FrameId> = (0..pool_size).collect();
        BufferPool {
            frames,
            page_table: HashMap::new(),
            free_list,
            replacer: LruReplacer::new(pool_size),
            disk,
        }
    }

    /// Fetch a page into the pool and pin it. Returns a `FrameId`.
    ///
    /// The caller must call `unpin_page` when finished with the page.
    /// Pinned frames are never evicted.
    pub fn fetch_page(&mut self, page_id: PageId) -> Result<FrameId> {
        // Fast path: page already in pool
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            self.frames[frame_id].pin_count += 1;
            self.replacer.set_evictable(frame_id, false);
            self.replacer.record_access(frame_id);
            return Ok(frame_id);
        }

        // Need a free frame
        let frame_id = self.find_free_frame()?;

        // Read page from disk into the frame
        let mut buf = [0u8; PAGE_SIZE];
        self.disk.read_page(page_id, &mut buf)?;

        self.frames[frame_id].page = Page::from_bytes(buf);
        self.frames[frame_id].page_id = Some(page_id);
        self.frames[frame_id].pin_count = 1;
        self.frames[frame_id].is_dirty = false;

        self.page_table.insert(page_id, frame_id);
        self.replacer.record_access(frame_id);
        self.replacer.set_evictable(frame_id, false);

        Ok(frame_id)
    }

    /// Allocate a new page on disk and load it into the pool (pinned).
    ///
    /// Returns `(PageId, FrameId)`. The caller must call `unpin_page` when done.
    pub fn new_page(&mut self) -> Result<(PageId, FrameId)> {
        let frame_id = self.find_free_frame()?;

        let page_id = self.disk.allocate_page()?;
        let page = Page::new(page_id);

        self.frames[frame_id].page = page;
        self.frames[frame_id].page_id = Some(page_id);
        self.frames[frame_id].pin_count = 1;
        self.frames[frame_id].is_dirty = true; // newly allocated, needs to be written

        self.page_table.insert(page_id, frame_id);
        self.replacer.record_access(frame_id);
        self.replacer.set_evictable(frame_id, false);

        Ok((page_id, frame_id))
    }

    /// Immutable access to the page in a frame.
    ///
    /// The frame must be pinned (i.e., `fetch_page` or `new_page` was called
    /// and `unpin_page` has not yet been called).
    pub fn page(&self, frame_id: FrameId) -> &Page {
        &self.frames[frame_id].page
    }

    /// Mutable access to the page in a frame.
    pub fn page_mut(&mut self, frame_id: FrameId) -> &mut Page {
        &mut self.frames[frame_id].page
    }

    /// Decrement the pin count for a page. Mark dirty if `is_dirty` is true.
    ///
    /// When pin_count reaches 0 the frame becomes eligible for eviction.
    pub fn unpin_page(&mut self, page_id: PageId, is_dirty: bool) -> Result<()> {
        let frame_id = *self
            .page_table
            .get(&page_id)
            .ok_or(StorageError::PageNotFound(page_id))?;

        if self.frames[frame_id].pin_count == 0 {
            return Err(StorageError::PageNotPinned(page_id));
        }

        self.frames[frame_id].pin_count -= 1;
        if is_dirty {
            self.frames[frame_id].is_dirty = true;
        }
        if self.frames[frame_id].pin_count == 0 {
            self.replacer.set_evictable(frame_id, true);
        }

        Ok(())
    }

    /// Write the page to disk if dirty. Keeps the page in the pool.
    pub fn flush_page(&mut self, page_id: PageId) -> Result<()> {
        let frame_id = *self
            .page_table
            .get(&page_id)
            .ok_or(StorageError::PageNotFound(page_id))?;

        if self.frames[frame_id].is_dirty {
            let data = *self.frames[frame_id].page.as_bytes();
            self.disk.write_page(page_id, &data)?;
            self.frames[frame_id].is_dirty = false;
        }

        Ok(())
    }

    /// Flush all dirty frames to disk.
    pub fn flush_all(&mut self) -> Result<()> {
        let dirty: Vec<PageId> = self
            .frames
            .iter()
            .filter_map(|f| {
                if f.is_dirty {
                    f.page_id
                } else {
                    None
                }
            })
            .collect();

        for page_id in dirty {
            self.flush_page(page_id)?;
        }

        Ok(())
    }

    /// Remove a page from the pool entirely (use when a page is deleted).
    ///
    /// The frame must not be pinned.
    pub fn delete_page(&mut self, page_id: PageId) -> Result<()> {
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            if self.frames[frame_id].pin_count > 0 {
                return Err(StorageError::Corruption(format!(
                    "cannot delete pinned page {}",
                    page_id
                )));
            }
            self.page_table.remove(&page_id);
            self.replacer.remove(frame_id);
            self.frames[frame_id].page_id = None;
            self.frames[frame_id].is_dirty = false;
            self.frames[frame_id].pin_count = 0;
            self.free_list.push_back(frame_id);
        }
        Ok(())
    }

    pub fn pool_size(&self) -> usize {
        self.frames.len()
    }

    pub fn free_frames(&self) -> usize {
        self.free_list.len()
    }

    /// Give access to the underlying DiskManager (e.g., for the WAL layer).
    pub fn disk(&self) -> &DiskManager {
        &self.disk
    }

    /// Returns true if the page is currently loaded in the pool (used by recovery).
    pub fn contains_page(&self, page_id: PageId) -> bool {
        self.page_table.contains_key(&page_id)
    }

    // --- Internal ---

    /// Return a frame_id ready for use, either from the free list or by evicting.
    fn find_free_frame(&mut self) -> Result<FrameId> {
        if let Some(frame_id) = self.free_list.pop_front() {
            return Ok(frame_id);
        }

        let frame_id = self.replacer.evict().ok_or(StorageError::BufferFull)?;

        // Flush the evicted frame if dirty
        if self.frames[frame_id].is_dirty {
            let page_id = self.frames[frame_id].page_id.unwrap();
            let data = *self.frames[frame_id].page.as_bytes();
            self.disk.write_page(page_id, &data)?;
            self.frames[frame_id].is_dirty = false;
        }

        // Remove evicted page from the page table
        if let Some(evicted_page_id) = self.frames[frame_id].page_id {
            self.page_table.remove(&evicted_page_id);
        }

        self.frames[frame_id].page_id = None;
        Ok(frame_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_pool(pool_size: usize) -> (BufferPool, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let disk = DiskManager::create(&path).unwrap();
        (BufferPool::new(pool_size, disk), dir)
    }

    #[test]
    fn test_new_page_and_fetch() {
        let (mut pool, _dir) = make_pool(10);
        let (page_id, frame_id) = pool.new_page().unwrap();
        assert_eq!(page_id, 1);

        // Write something to the page
        pool.page_mut(frame_id).insert_tuple(b"hello").unwrap();
        pool.unpin_page(page_id, true).unwrap();

        // Fetch it back
        let frame_id2 = pool.fetch_page(page_id).unwrap();
        assert_eq!(pool.page(frame_id2).get_tuple(0).unwrap(), b"hello");
        pool.unpin_page(page_id, false).unwrap();
    }

    #[test]
    fn test_fetch_same_page_twice_increments_pin_count() {
        let (mut pool, _dir) = make_pool(10);
        let (page_id, _) = pool.new_page().unwrap();
        pool.unpin_page(page_id, false).unwrap();

        let fid1 = pool.fetch_page(page_id).unwrap();
        let fid2 = pool.fetch_page(page_id).unwrap();
        assert_eq!(fid1, fid2); // same frame
        // Two pins — need two unpins
        pool.unpin_page(page_id, false).unwrap();
        pool.unpin_page(page_id, false).unwrap();
    }

    #[test]
    fn test_pool_exhaustion_returns_error() {
        let (mut pool, _dir) = make_pool(3);
        let mut ids = Vec::new();
        for _ in 0..3 {
            let (pid, _) = pool.new_page().unwrap();
            ids.push(pid);
            // Keep all pinned — do not unpin
        }
        // All 3 frames pinned, no eviction possible
        assert!(matches!(pool.new_page(), Err(StorageError::BufferFull)));
    }

    #[test]
    fn test_eviction_writes_dirty_page() {
        let (mut pool, _dir) = make_pool(2);

        // Fill both frames with dirty pages
        let (pid1, fid1) = pool.new_page().unwrap();
        pool.page_mut(fid1).insert_tuple(b"page1").unwrap();
        pool.unpin_page(pid1, true).unwrap(); // dirty, evictable

        let (pid2, fid2) = pool.new_page().unwrap();
        pool.page_mut(fid2).insert_tuple(b"page2").unwrap();
        pool.unpin_page(pid2, true).unwrap(); // dirty, evictable

        // Fetch a third page — forces eviction of pid1 (LRU)
        let (pid3, _) = pool.new_page().unwrap();

        // Now fetch pid1 from disk — should have been written before eviction
        pool.unpin_page(pid3, false).unwrap();
        let fid = pool.fetch_page(pid1).unwrap();
        assert_eq!(pool.page(fid).get_tuple(0).unwrap(), b"page1");
        pool.unpin_page(pid1, false).unwrap();
    }

    #[test]
    fn test_flush_all_persists_dirty_pages() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        let pid;
        {
            let disk = DiskManager::create(&path).unwrap();
            let mut pool = BufferPool::new(10, disk);
            let (page_id, fid) = pool.new_page().unwrap();
            pid = page_id;
            pool.page_mut(fid).insert_tuple(b"flush test").unwrap();
            pool.unpin_page(page_id, true).unwrap();
            pool.flush_all().unwrap();
        }

        // Reopen and verify
        let disk2 = DiskManager::open(&path).unwrap();
        let mut pool2 = BufferPool::new(10, disk2);
        let fid = pool2.fetch_page(pid).unwrap();
        assert_eq!(pool2.page(fid).get_tuple(0).unwrap(), b"flush test");
    }

    #[test]
    fn test_unpin_nonexistent_page_errors() {
        let (mut pool, _dir) = make_pool(5);
        assert!(matches!(
            pool.unpin_page(999, false),
            Err(StorageError::PageNotFound(999))
        ));
    }

    #[test]
    fn test_lru_eviction_order() {
        let (mut pool, _dir) = make_pool(3);

        let (pid1, _) = pool.new_page().unwrap();
        let (pid2, _) = pool.new_page().unwrap();
        let (pid3, _) = pool.new_page().unwrap();

        // Unpin all — pid1 was accessed first (LRU)
        pool.unpin_page(pid1, false).unwrap();
        pool.unpin_page(pid2, false).unwrap();
        pool.unpin_page(pid3, false).unwrap();

        // Re-access pid1 — now pid2 is LRU
        pool.fetch_page(pid1).unwrap();
        pool.unpin_page(pid1, false).unwrap();

        // Allocating a 4th page forces eviction — should evict pid2
        let (pid4, _fid4) = pool.new_page().unwrap();
        assert!(pool.page_table.contains_key(&pid1));
        assert!(!pool.page_table.contains_key(&pid2)); // evicted
        assert!(pool.page_table.contains_key(&pid3));
        pool.unpin_page(pid4, false).unwrap();
    }

    #[test]
    fn test_milestone_write_and_read_many_pages() {
        // Month 1 milestone: 50 pages written with known data, read back correctly
        // using a small pool that must evict
        let (mut pool, _dir) = make_pool(10);

        let mut pages: Vec<(PageId, Vec<u8>)> = Vec::new();

        // Write 50 pages
        for i in 0u8..50 {
            let (pid, fid) = pool.new_page().unwrap();
            let data: Vec<u8> = vec![i; 64];
            pool.page_mut(fid).insert_tuple(&data).unwrap();
            pool.unpin_page(pid, true).unwrap();
            pages.push((pid, data));
        }

        pool.flush_all().unwrap();

        // Read all 50 pages back and verify
        for (pid, expected) in &pages {
            let fid = pool.fetch_page(*pid).unwrap();
            let actual = pool.page(fid).get_tuple(0).unwrap();
            assert_eq!(actual, expected.as_slice(), "mismatch at page {}", pid);
            pool.unpin_page(*pid, false).unwrap();
        }
    }
}
