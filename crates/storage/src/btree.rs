use crate::buffer_pool::BufferPool;
use crate::error::{Result, StorageError};
use crate::heap::TupleId;
use crate::page::{
    PageId, BTREE_FLAG_IS_LEAF, BTREE_FLAG_IS_ROOT, BTREE_FLAG_IS_RIGHTMOST,
    BTREE_SPECIAL_SIZE, INVALID_PAGE_ID, PAGE_HEADER_SIZE, PAGE_SIZE, SLOT_SIZE,
};

// Meta page layout:  magic(4) + root_page_id(8) + height(4) + num_entries(8) = 24 bytes
const META_MAGIC: u32 = 0x454D_5442; // "BTME"
const META_OFF_MAGIC: usize = 0;
const META_OFF_ROOT: usize = 4;
const META_OFF_HEIGHT: usize = 12;
const META_OFF_NENTRIES: usize = 16;

/// Usable bytes per B+ tree node page (header + special excluded).
const NODE_CAPACITY: usize = PAGE_SIZE - PAGE_HEADER_SIZE - BTREE_SPECIAL_SIZE;

/// A B+ tree index: keys are raw byte slices, values are TupleIds (leaf) or PageIds (internal).
///
/// Keys must be in the caller's chosen canonical byte ordering (e.g., big-endian u64 for integers).
pub struct BTree {
    meta_page_id: PageId,
    root_page_id: PageId,
}

impl BTree {
    /// Create a new, empty B+ tree backed by `pool`. Returns the new tree.
    /// The caller must store `meta_page_id()` to reopen the tree later.
    pub fn create(pool: &mut BufferPool) -> Result<Self> {
        // Allocate meta page
        let (meta_page_id, _meta_fid) = pool.new_page()?;

        // Allocate initial root leaf
        let (root_page_id, root_fid) = pool.new_page()?;
        pool.page_mut(root_fid).init_btree_page(true, true);
        pool.unpin_page(root_page_id, true)?;

        // Write meta
        let meta_fid = pool.fetch_page(meta_page_id)?;
        let d = pool.page_mut(meta_fid).as_bytes_mut();
        d[META_OFF_MAGIC..META_OFF_MAGIC + 4].copy_from_slice(&META_MAGIC.to_le_bytes());
        d[META_OFF_ROOT..META_OFF_ROOT + 8].copy_from_slice(&root_page_id.to_le_bytes());
        d[META_OFF_HEIGHT..META_OFF_HEIGHT + 4].copy_from_slice(&0u32.to_le_bytes());
        d[META_OFF_NENTRIES..META_OFF_NENTRIES + 8].copy_from_slice(&0u64.to_le_bytes());
        pool.unpin_page(meta_page_id, true)?;

        Ok(BTree { meta_page_id, root_page_id })
    }

    /// Open an existing B+ tree by its meta page.
    pub fn open(pool: &mut BufferPool, meta_page_id: PageId) -> Result<Self> {
        let fid = pool.fetch_page(meta_page_id)?;
        let d = pool.page(fid).as_bytes();
        let magic = u32::from_le_bytes(d[META_OFF_MAGIC..META_OFF_MAGIC + 4].try_into().unwrap());
        if magic != META_MAGIC {
            pool.unpin_page(meta_page_id, false)?;
            return Err(StorageError::Corruption("invalid B+ tree meta page".into()));
        }
        let root_page_id =
            u64::from_le_bytes(d[META_OFF_ROOT..META_OFF_ROOT + 8].try_into().unwrap());
        pool.unpin_page(meta_page_id, false)?;
        Ok(BTree { meta_page_id, root_page_id })
    }

    pub fn meta_page_id(&self) -> PageId {
        self.meta_page_id
    }

    /// Insert `key -> tid` into the tree.
    pub fn insert(&mut self, pool: &mut BufferPool, key: &[u8], tid: TupleId) -> Result<()> {
        let split = self.insert_recursive(pool, self.root_page_id, key, tid)?;

        if let Some((sep_key, new_page_id)) = split {
            // Root split — create a new root above the two halves
            let old_root_level = {
                let fid = pool.fetch_page(self.root_page_id)?;
                let lvl = pool.page(fid).btree_level();
                pool.unpin_page(self.root_page_id, false)?;
                lvl
            };

            // Clear IS_ROOT from old root
            let fid = pool.fetch_page(self.root_page_id)?;
            let flags = pool.page(fid).btree_flags();
            pool.page_mut(fid).set_btree_flags(flags & !BTREE_FLAG_IS_ROOT);
            pool.unpin_page(self.root_page_id, true)?;

            // Build new root
            let (new_root_id, new_root_fid) = pool.new_page()?;
            pool.page_mut(new_root_fid).init_btree_page(false, true);
            pool.page_mut(new_root_fid).set_btree_level(old_root_level + 1);
            pool.page_mut(new_root_fid).set_btree_leftmost_child(self.root_page_id);

            let entry = encode_internal_entry(&sep_key, new_page_id);
            pool.page_mut(new_root_fid).insert_tuple_at(0, &entry)?;
            pool.unpin_page(new_root_id, true)?;

            self.root_page_id = new_root_id;
            self.persist_root(pool)?;
        }

        Ok(())
    }

    /// Look up an exact key. Returns the TupleId or None if not found.
    pub fn lookup(&self, pool: &mut BufferPool, key: &[u8]) -> Result<Option<TupleId>> {
        let mut page_id = self.root_page_id;
        loop {
            let fid = pool.fetch_page(page_id)?;
            let is_leaf = pool.page(fid).btree_flags() & BTREE_FLAG_IS_LEAF != 0;

            if is_leaf {
                let result = find_in_leaf(pool.page(fid), key);
                pool.unpin_page(page_id, false)?;
                return Ok(result);
            }

            let child = descend(pool.page(fid), key);
            pool.unpin_page(page_id, false)?;
            page_id = child;
        }
    }

    /// Scan all entries with `start <= key` and (if `end` is given) `key < end`.
    /// Returns entries in ascending key order.
    pub fn range_scan(
        &self,
        pool: &mut BufferPool,
        start: &[u8],
        end: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, TupleId)>> {
        // Descend to the leaf that would contain `start`
        let mut page_id = self.root_page_id;
        loop {
            let fid = pool.fetch_page(page_id)?;
            let is_leaf = pool.page(fid).btree_flags() & BTREE_FLAG_IS_LEAF != 0;
            if is_leaf {
                pool.unpin_page(page_id, false)?;
                break;
            }
            let child = descend(pool.page(fid), start);
            pool.unpin_page(page_id, false)?;
            page_id = child;
        }

        let mut results = Vec::new();
        'outer: loop {
            let fid = pool.fetch_page(page_id)?;
            let next = pool.page(fid).btree_next_page();
            let slot_count = pool.page(fid).slot_count();

            for slot_id in 0..slot_count {
                if !pool.page(fid).is_slot_used(slot_id) {
                    continue;
                }
                let bytes = pool.page(fid).get_tuple(slot_id)?.to_vec();
                let (k, tid) = decode_leaf_entry(&bytes)?;

                if k.as_slice() < start {
                    continue;
                }
                if let Some(e) = end {
                    if k.as_slice() >= e {
                        pool.unpin_page(page_id, false)?;
                        break 'outer;
                    }
                }
                results.push((k, tid));
            }

            pool.unpin_page(page_id, false)?;
            if next == INVALID_PAGE_ID {
                break;
            }
            page_id = next;
        }

        Ok(results)
    }

    // --- Internal recursive insert ---

    fn insert_recursive(
        &mut self,
        pool: &mut BufferPool,
        page_id: PageId,
        key: &[u8],
        tid: TupleId,
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        let fid = pool.fetch_page(page_id)?;
        let is_leaf = pool.page(fid).btree_flags() & BTREE_FLAG_IS_LEAF != 0;
        pool.unpin_page(page_id, false)?;

        if is_leaf {
            self.leaf_insert(pool, page_id, key, tid)
        } else {
            let child = {
                let fid = pool.fetch_page(page_id)?;
                let c = descend(pool.page(fid), key);
                pool.unpin_page(page_id, false)?;
                c
            };

            let split = self.insert_recursive(pool, child, key, tid)?;
            if let Some((sep_key, new_child)) = split {
                self.internal_insert(pool, page_id, &sep_key, new_child)
            } else {
                Ok(None)
            }
        }
    }

    fn leaf_insert(
        &mut self,
        pool: &mut BufferPool,
        page_id: PageId,
        key: &[u8],
        tid: TupleId,
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        let entry = encode_leaf_entry(key, tid);
        let needed = SLOT_SIZE + entry.len();

        let fid = pool.fetch_page(page_id)?;
        let pos = leaf_insert_pos(pool.page(fid), key);
        let has_space = pool.page(fid).free_space() >= needed;

        if has_space {
            pool.page_mut(fid).insert_tuple_at(pos, &entry)?;
            pool.unpin_page(page_id, true)?;
            return Ok(None);
        }

        // Collect all current entries then unpin
        let existing = collect_leaf_entries(pool.page(fid));
        pool.unpin_page(page_id, false)?;

        // Merge new entry in sorted order
        let mut all: Vec<(Vec<u8>, TupleId)> = existing;
        let ins = all.partition_point(|(k, _)| k.as_slice() <= key);
        all.insert(ins, (key.to_vec(), tid));

        let mid = all.len() / 2;
        let separator_key = all[mid].0.clone();

        // Rewrite old page with first half; also update sibling links
        let (new_page_id, new_fid) = pool.new_page()?;

        let fid = pool.fetch_page(page_id)?;
        let old_next = pool.page(fid).btree_next_page();
        let old_flags = pool.page(fid).btree_flags();
        pool.page_mut(fid).clear_tuples();
        pool.page_mut(fid).set_btree_flags(old_flags & !BTREE_FLAG_IS_RIGHTMOST);
        pool.page_mut(fid).set_btree_next_page(new_page_id);
        for (k, t) in &all[..mid] {
            let e = encode_leaf_entry(k, *t);
            let sc = pool.page(fid).slot_count();
            pool.page_mut(fid).insert_tuple_at(sc, &e)?;
        }
        pool.unpin_page(page_id, true)?;

        // Fill new page with second half
        pool.page_mut(new_fid).init_btree_page(true, false);
        pool.page_mut(new_fid).set_btree_prev_page(page_id);
        pool.page_mut(new_fid).set_btree_next_page(old_next);
        for (k, t) in &all[mid..] {
            let e = encode_leaf_entry(k, *t);
            let sc = pool.page(new_fid).slot_count();
            pool.page_mut(new_fid).insert_tuple_at(sc, &e)?;
        }
        pool.unpin_page(new_page_id, true)?;

        // Fix old_next's prev pointer
        if old_next != INVALID_PAGE_ID {
            let on_fid = pool.fetch_page(old_next)?;
            pool.page_mut(on_fid).set_btree_prev_page(new_page_id);
            pool.unpin_page(old_next, true)?;
        }

        Ok(Some((separator_key, new_page_id)))
    }

    fn internal_insert(
        &mut self,
        pool: &mut BufferPool,
        page_id: PageId,
        sep_key: &[u8],
        new_child: PageId,
    ) -> Result<Option<(Vec<u8>, PageId)>> {
        let entry = encode_internal_entry(sep_key, new_child);
        let needed = SLOT_SIZE + entry.len();

        let fid = pool.fetch_page(page_id)?;
        let pos = internal_insert_pos(pool.page(fid), sep_key);
        let has_space = pool.page(fid).free_space() >= needed;

        if has_space {
            pool.page_mut(fid).insert_tuple_at(pos, &entry)?;
            pool.unpin_page(page_id, true)?;
            return Ok(None);
        }

        // Collect all entries and split
        let leftmost = pool.page(fid).btree_leftmost_child();
        let level = pool.page(fid).btree_level();
        let existing = collect_internal_entries(pool.page(fid));
        pool.unpin_page(page_id, false)?;

        let mut all: Vec<(Vec<u8>, PageId)> = existing;
        let ins = all.partition_point(|(k, _)| k.as_slice() <= sep_key);
        all.insert(ins, (sep_key.to_vec(), new_child));

        // mid entry is the push-up separator; left page gets [0..mid), right gets [mid+1..)
        let mid = all.len() / 2;
        let push_up_key = all[mid].0.clone();
        let new_leftmost_right = all[mid].1;

        // Rewrite old page with first half
        let fid = pool.fetch_page(page_id)?;
        pool.page_mut(fid).clear_tuples();
        pool.page_mut(fid).set_btree_leftmost_child(leftmost);
        for (k, child) in &all[..mid] {
            let e = encode_internal_entry(k, *child);
            let sc = pool.page(fid).slot_count();
            pool.page_mut(fid).insert_tuple_at(sc, &e)?;
        }
        pool.unpin_page(page_id, true)?;

        // New page with second half
        let (new_page_id, new_fid) = pool.new_page()?;
        pool.page_mut(new_fid).init_btree_page(false, false);
        pool.page_mut(new_fid).set_btree_level(level);
        pool.page_mut(new_fid).set_btree_leftmost_child(new_leftmost_right);
        for (k, child) in &all[mid + 1..] {
            let e = encode_internal_entry(k, *child);
            let sc = pool.page(new_fid).slot_count();
            pool.page_mut(new_fid).insert_tuple_at(sc, &e)?;
        }
        pool.unpin_page(new_page_id, true)?;

        Ok(Some((push_up_key, new_page_id)))
    }

    fn persist_root(&mut self, pool: &mut BufferPool) -> Result<()> {
        let fid = pool.fetch_page(self.meta_page_id)?;
        pool.page_mut(fid).as_bytes_mut()[META_OFF_ROOT..META_OFF_ROOT + 8]
            .copy_from_slice(&self.root_page_id.to_le_bytes());
        pool.unpin_page(self.meta_page_id, true)?;
        Ok(())
    }
}

// --- Entry encoding / decoding ---

/// Leaf entry wire format: key_len(u16) + key + page_id(u64) + slot_id(u16) = 12 + key_len bytes
fn encode_leaf_entry(key: &[u8], tid: TupleId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(12 + key.len());
    buf.extend_from_slice(&(key.len() as u16).to_le_bytes());
    buf.extend_from_slice(key);
    buf.extend_from_slice(&tid.page_id.to_le_bytes());
    buf.extend_from_slice(&tid.slot_id.to_le_bytes());
    buf
}

fn decode_leaf_entry(data: &[u8]) -> Result<(Vec<u8>, TupleId)> {
    if data.len() < 12 {
        return Err(StorageError::Corruption("short btree leaf entry".into()));
    }
    let key_len = u16::from_le_bytes(data[0..2].try_into().unwrap()) as usize;
    if data.len() < 2 + key_len + 10 {
        return Err(StorageError::Corruption("truncated btree leaf entry".into()));
    }
    let key = data[2..2 + key_len].to_vec();
    let page_id = u64::from_le_bytes(data[2 + key_len..2 + key_len + 8].try_into().unwrap());
    let slot_id =
        u16::from_le_bytes(data[2 + key_len + 8..2 + key_len + 10].try_into().unwrap());
    Ok((key, TupleId { page_id, slot_id }))
}

/// Internal entry wire format: key_len(u16) + key + child_page(u64) = 10 + key_len bytes
fn encode_internal_entry(key: &[u8], child: PageId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(10 + key.len());
    buf.extend_from_slice(&(key.len() as u16).to_le_bytes());
    buf.extend_from_slice(key);
    buf.extend_from_slice(&child.to_le_bytes());
    buf
}

fn decode_internal_entry(data: &[u8]) -> Result<(Vec<u8>, PageId)> {
    if data.len() < 10 {
        return Err(StorageError::Corruption("short btree internal entry".into()));
    }
    let key_len = u16::from_le_bytes(data[0..2].try_into().unwrap()) as usize;
    if data.len() < 2 + key_len + 8 {
        return Err(StorageError::Corruption("truncated btree internal entry".into()));
    }
    let key = data[2..2 + key_len].to_vec();
    let child = u64::from_le_bytes(data[2 + key_len..2 + key_len + 8].try_into().unwrap());
    Ok((key, child))
}

// --- Node navigation helpers ---

/// Return the child page to descend into for `key` within an internal node.
/// Slots are sorted separator keys; leftmost_child covers keys below slot[0].
fn descend(page: &crate::page::Page, key: &[u8]) -> PageId {
    let slot_count = page.slot_count();
    if slot_count == 0 {
        return page.btree_leftmost_child();
    }
    // Binary search: find first slot whose separator key > key
    let mut lo = 0u16;
    let mut hi = slot_count;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if !page.is_slot_used(mid) {
            hi = mid; // skip
            continue;
        }
        let entry = page.get_tuple(mid).unwrap();
        let kl = u16::from_le_bytes(entry[0..2].try_into().unwrap()) as usize;
        let sep = &entry[2..2 + kl];
        if sep <= key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    // lo is the first slot with sep > key; go to child at slot lo-1 or leftmost
    if lo == 0 {
        page.btree_leftmost_child()
    } else {
        let entry = page.get_tuple(lo - 1).unwrap();
        let kl = u16::from_le_bytes(entry[0..2].try_into().unwrap()) as usize;
        u64::from_le_bytes(entry[2 + kl..2 + kl + 8].try_into().unwrap())
    }
}

fn find_in_leaf(page: &crate::page::Page, key: &[u8]) -> Option<TupleId> {
    for i in 0..page.slot_count() {
        if !page.is_slot_used(i) {
            continue;
        }
        let entry = page.get_tuple(i).unwrap();
        let kl = u16::from_le_bytes(entry[0..2].try_into().unwrap()) as usize;
        let existing = &entry[2..2 + kl];
        if existing == key {
            let pid = u64::from_le_bytes(entry[2 + kl..2 + kl + 8].try_into().unwrap());
            let sid = u16::from_le_bytes(entry[2 + kl + 8..2 + kl + 10].try_into().unwrap());
            return Some(TupleId { page_id: pid, slot_id: sid });
        }
        if existing > key {
            break; // sorted order — not present
        }
    }
    None
}

/// Insertion position to keep the leaf sorted.
fn leaf_insert_pos(page: &crate::page::Page, key: &[u8]) -> u16 {
    for i in 0..page.slot_count() {
        if !page.is_slot_used(i) {
            continue;
        }
        let entry = page.get_tuple(i).unwrap();
        let kl = u16::from_le_bytes(entry[0..2].try_into().unwrap()) as usize;
        if &entry[2..2 + kl] > key {
            return i;
        }
    }
    page.slot_count()
}

fn internal_insert_pos(page: &crate::page::Page, key: &[u8]) -> u16 {
    for i in 0..page.slot_count() {
        if !page.is_slot_used(i) {
            continue;
        }
        let entry = page.get_tuple(i).unwrap();
        let kl = u16::from_le_bytes(entry[0..2].try_into().unwrap()) as usize;
        if &entry[2..2 + kl] > key {
            return i;
        }
    }
    page.slot_count()
}

fn collect_leaf_entries(page: &crate::page::Page) -> Vec<(Vec<u8>, TupleId)> {
    let mut out = Vec::new();
    for i in 0..page.slot_count() {
        if !page.is_slot_used(i) {
            continue;
        }
        let entry = page.get_tuple(i).unwrap();
        out.push(decode_leaf_entry(entry).unwrap());
    }
    out
}

fn collect_internal_entries(page: &crate::page::Page) -> Vec<(Vec<u8>, PageId)> {
    let mut out = Vec::new();
    for i in 0..page.slot_count() {
        if !page.is_slot_used(i) {
            continue;
        }
        let entry = page.get_tuple(i).unwrap();
        out.push(decode_internal_entry(entry).unwrap());
    }
    out
}

/// Encode a u64 key as 8 big-endian bytes so lexicographic order == numeric order.
pub fn encode_u64_key(v: u64) -> [u8; 8] {
    v.to_be_bytes()
}

pub fn decode_u64_key(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

// --- unused import guard ---
#[allow(dead_code)]
const _NODE_CAPACITY: usize = NODE_CAPACITY;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::DiskManager;
    use crate::heap::TupleId;
    use tempfile::tempdir;

    fn make_pool(frames: usize) -> (BufferPool, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let disk = DiskManager::create(&dir.path().join("test.db")).unwrap();
        (BufferPool::new(frames, disk), dir)
    }

    fn fake_tid(n: u64) -> TupleId {
        TupleId { page_id: n, slot_id: (n % 500) as u16 }
    }

    #[test]
    fn test_create_and_lookup_single_key() {
        let (mut pool, _dir) = make_pool(32);
        let mut tree = BTree::create(&mut pool).unwrap();

        let key = encode_u64_key(42);
        let tid = fake_tid(42);
        tree.insert(&mut pool, &key, tid).unwrap();

        let found = tree.lookup(&mut pool, &key).unwrap();
        assert_eq!(found, Some(tid));

        let missing = tree.lookup(&mut pool, &encode_u64_key(99)).unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_insert_many_and_lookup_all() {
        let (mut pool, _dir) = make_pool(64);
        let mut tree = BTree::create(&mut pool).unwrap();

        let n = 500u64;
        for i in 0..n {
            tree.insert(&mut pool, &encode_u64_key(i), fake_tid(i)).unwrap();
        }
        for i in 0..n {
            let found = tree.lookup(&mut pool, &encode_u64_key(i)).unwrap();
            assert_eq!(found, Some(fake_tid(i)), "missing key {}", i);
        }
    }

    #[test]
    fn test_range_scan_returns_sorted_subset() {
        let (mut pool, _dir) = make_pool(64);
        let mut tree = BTree::create(&mut pool).unwrap();

        for i in 0u64..100 {
            tree.insert(&mut pool, &encode_u64_key(i), fake_tid(i)).unwrap();
        }

        let start = encode_u64_key(20);
        let end = encode_u64_key(30);
        let results = tree.range_scan(&mut pool, &start, Some(&end)).unwrap();

        assert_eq!(results.len(), 10); // [20, 29]
        for (i, (k, _)) in results.iter().enumerate() {
            assert_eq!(decode_u64_key(k), 20 + i as u64);
        }
    }

    #[test]
    fn test_range_scan_open_ended() {
        let (mut pool, _dir) = make_pool(64);
        let mut tree = BTree::create(&mut pool).unwrap();

        for i in 0u64..50 {
            tree.insert(&mut pool, &encode_u64_key(i), fake_tid(i)).unwrap();
        }

        let start = encode_u64_key(45);
        let results = tree.range_scan(&mut pool, &start, None).unwrap();
        assert_eq!(results.len(), 5); // [45..49]
    }

    #[test]
    fn test_tree_survives_many_splits() {
        // Forces multiple levels of splits with a small pool
        let (mut pool, _dir) = make_pool(50);
        let mut tree = BTree::create(&mut pool).unwrap();

        let n = 5_000u64;
        for i in 0..n {
            tree.insert(&mut pool, &encode_u64_key(i), fake_tid(i)).unwrap();
        }
        for i in 0..n {
            let found = tree.lookup(&mut pool, &encode_u64_key(i)).unwrap();
            assert_eq!(found, Some(fake_tid(i)), "missing key {}", i);
        }

        // Full range scan must return all n entries in order
        let all = tree.range_scan(&mut pool, &encode_u64_key(0), None).unwrap();
        assert_eq!(all.len() as u64, n);
        for (i, (k, _)) in all.iter().enumerate() {
            assert_eq!(decode_u64_key(k), i as u64);
        }
    }

    #[test]
    fn test_open_restores_tree() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let meta_page_id;

        {
            let disk = DiskManager::create(&db_path).unwrap();
            let mut pool = BufferPool::new(64, disk);
            let mut tree = BTree::create(&mut pool).unwrap();
            meta_page_id = tree.meta_page_id();

            for i in 0u64..200 {
                tree.insert(&mut pool, &encode_u64_key(i), fake_tid(i)).unwrap();
            }
            pool.flush_all().unwrap();
        }

        // Reopen
        let disk2 = DiskManager::open(&db_path).unwrap();
        let mut pool2 = BufferPool::new(64, disk2);
        let tree2 = BTree::open(&mut pool2, meta_page_id).unwrap();

        for i in 0u64..200 {
            let found = tree2.lookup(&mut pool2, &encode_u64_key(i)).unwrap();
            assert_eq!(found, Some(fake_tid(i)), "after reopen, missing key {}", i);
        }
    }

    #[test]
    fn test_milestone_large_insert_and_scan() {
        // Month 4 milestone: 10,000 entries, small pool, all lookups correct
        let (mut pool, _dir) = make_pool(100);
        let mut tree = BTree::create(&mut pool).unwrap();

        let n = 10_000u64;
        // Insert in reverse to stress split handling
        for i in (0..n).rev() {
            tree.insert(&mut pool, &encode_u64_key(i), fake_tid(i)).unwrap();
        }
        for i in 0..n {
            let found = tree.lookup(&mut pool, &encode_u64_key(i)).unwrap();
            assert_eq!(found, Some(fake_tid(i)), "missing key {}", i);
        }

        // Range scan returns all in order
        let all = tree.range_scan(&mut pool, &encode_u64_key(0), None).unwrap();
        assert_eq!(all.len() as u64, n);
        for (i, (k, _)) in all.iter().enumerate() {
            assert_eq!(decode_u64_key(k), i as u64);
        }
    }
}
