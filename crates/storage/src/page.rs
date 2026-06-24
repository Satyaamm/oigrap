use crate::error::{Result, StorageError};

pub const PAGE_SIZE: usize = 8192;
pub const PAGE_HEADER_SIZE: usize = 48;
pub const SLOT_SIZE: usize = 4;

pub type PageId = u64;
pub const INVALID_PAGE_ID: PageId = u64::MAX;

// Page flags
pub const FLAG_HAS_FREE_LINES: u16 = 0x0001;
pub const FLAG_PAGE_FULL: u16 = 0x0002;
pub const FLAG_ALL_VISIBLE: u16 = 0x0004;

// Tuple infomask flags
pub const HEAP_HAS_NULL: u16 = 0x0001;
pub const HEAP_HAS_VARWIDTH: u16 = 0x0002;
pub const HEAP_XMIN_COMMITTED: u16 = 0x0100;
pub const HEAP_XMIN_INVALID: u16 = 0x0200;
pub const HEAP_XMAX_COMMITTED: u16 = 0x0400;
pub const HEAP_XMAX_INVALID: u16 = 0x0800;
pub const HEAP_UPDATED: u16 = 0x2000;

// Minimum tuple payload (24-byte MVCC header before column data)
pub const TUPLE_HEADER_SIZE: usize = 24;

// B+ tree special space constants
pub const BTREE_SPECIAL_SIZE: usize = 28;
pub const BTREE_FLAG_IS_ROOT: u16 = 0x0001;
pub const BTREE_FLAG_IS_LEAF: u16 = 0x0002;
pub const BTREE_FLAG_IS_RIGHTMOST: u16 = 0x0004;

// B+ tree special space byte offsets (relative to special start)
const BT_FLAGS: usize = 0;
const BT_LEVEL: usize = 2;
const BT_PREV: usize = 4;
const BT_NEXT: usize = 12;
const BT_LEFTMOST: usize = 20;

// Page header byte offsets (all little-endian)
const HDR_PAGE_ID: usize = 0;
const HDR_LSN: usize = 8;
const HDR_CHECKSUM: usize = 16;
const HDR_FLAGS: usize = 20;
const HDR_LOWER: usize = 22;
const HDR_UPPER: usize = 24;
const HDR_SPECIAL: usize = 26;
const HDR_XID_BASE: usize = 28;
const HDR_PRUNE_XID: usize = 36;

/// An 8KB database page. All on-disk data lives in pages.
///
/// Layout: header (48 bytes) | slot array (grows down) | free space | tuple data (grows up from end).
pub struct Page {
    data: [u8; PAGE_SIZE],
}

impl Page {
    /// Create a fresh, empty page with the given ID.
    pub fn new(page_id: PageId) -> Self {
        let mut p = Page { data: [0u8; PAGE_SIZE] };
        p.set_page_id(page_id);
        p.set_lower(PAGE_HEADER_SIZE as u16);
        p.set_upper(PAGE_SIZE as u16);
        p.set_special(PAGE_SIZE as u16);
        p
    }

    /// Reconstruct a page from raw bytes read from disk.
    pub fn from_bytes(bytes: [u8; PAGE_SIZE]) -> Self {
        Page { data: bytes }
    }

    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.data
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        &mut self.data
    }

    // --- Header accessors ---

    pub fn page_id(&self) -> PageId {
        read_u64(&self.data, HDR_PAGE_ID)
    }

    pub fn set_page_id(&mut self, id: PageId) {
        write_u64(&mut self.data, HDR_PAGE_ID, id);
    }

    pub fn lsn(&self) -> u64 {
        read_u64(&self.data, HDR_LSN)
    }

    pub fn set_lsn(&mut self, lsn: u64) {
        write_u64(&mut self.data, HDR_LSN, lsn);
    }

    pub fn checksum(&self) -> u32 {
        read_u32(&self.data, HDR_CHECKSUM)
    }

    pub fn flags(&self) -> u16 {
        read_u16(&self.data, HDR_FLAGS)
    }

    pub fn set_flags(&mut self, flags: u16) {
        write_u16(&mut self.data, HDR_FLAGS, flags);
    }

    pub fn lower(&self) -> u16 {
        read_u16(&self.data, HDR_LOWER)
    }

    fn set_lower(&mut self, lower: u16) {
        write_u16(&mut self.data, HDR_LOWER, lower);
    }

    pub fn upper(&self) -> u16 {
        read_u16(&self.data, HDR_UPPER)
    }

    fn set_upper(&mut self, upper: u16) {
        write_u16(&mut self.data, HDR_UPPER, upper);
    }

    pub fn special(&self) -> u16 {
        read_u16(&self.data, HDR_SPECIAL)
    }

    pub fn set_special(&mut self, special: u16) {
        write_u16(&mut self.data, HDR_SPECIAL, special);
    }

    pub fn xid_base(&self) -> u64 {
        read_u64(&self.data, HDR_XID_BASE)
    }

    pub fn set_xid_base(&mut self, xid_base: u64) {
        write_u64(&mut self.data, HDR_XID_BASE, xid_base);
    }

    pub fn prune_xid(&self) -> u32 {
        read_u32(&self.data, HDR_PRUNE_XID)
    }

    pub fn set_prune_xid(&mut self, xid: u32) {
        write_u32(&mut self.data, HDR_PRUNE_XID, xid);
    }

    // --- Slot and free space ---

    /// Bytes available for new slot + tuple data.
    pub fn free_space(&self) -> usize {
        let lower = self.lower() as usize;
        let upper = self.upper() as usize;
        upper.saturating_sub(lower)
    }

    /// Number of slots currently allocated (including dead slots).
    pub fn slot_count(&self) -> u16 {
        ((self.lower() as usize - PAGE_HEADER_SIZE) / SLOT_SIZE) as u16
    }

    pub fn is_slot_used(&self, slot_id: u16) -> bool {
        if slot_id >= self.slot_count() {
            return false;
        }
        let (offset, length) = self.read_slot(slot_id);
        offset != 0 || length != 0
    }

    fn read_slot(&self, slot_id: u16) -> (u16, u16) {
        let base = PAGE_HEADER_SIZE + slot_id as usize * SLOT_SIZE;
        let offset = read_u16(&self.data, base);
        let length = read_u16(&self.data, base + 2);
        (offset, length)
    }

    fn write_slot(&mut self, slot_id: u16, offset: u16, length: u16) {
        let base = PAGE_HEADER_SIZE + slot_id as usize * SLOT_SIZE;
        write_u16(&mut self.data, base, offset);
        write_u16(&mut self.data, base + 2, length);
    }

    // --- Slotted page operations ---

    /// Insert raw bytes as a new tuple. Returns the slot ID.
    ///
    /// The caller is responsible for including any MVCC tuple header in `data`.
    pub fn insert_tuple(&mut self, data: &[u8]) -> Result<u16> {
        let needed = SLOT_SIZE + data.len();
        let available = self.free_space();
        if available < needed {
            return Err(StorageError::InsufficientSpace { needed, available });
        }

        let new_upper = self.upper() as usize - data.len();
        self.data[new_upper..new_upper + data.len()].copy_from_slice(data);

        let slot_id = self.slot_count();
        self.write_slot(slot_id, new_upper as u16, data.len() as u16);

        self.set_upper(new_upper as u16);
        self.set_lower(self.lower() + SLOT_SIZE as u16);

        Ok(slot_id)
    }

    /// Return a reference to the raw bytes of a tuple at `slot_id`.
    pub fn get_tuple(&self, slot_id: u16) -> Result<&[u8]> {
        if slot_id >= self.slot_count() {
            return Err(StorageError::SlotOutOfRange(slot_id));
        }
        let (offset, length) = self.read_slot(slot_id);
        if offset == 0 && length == 0 {
            return Err(StorageError::SlotOutOfRange(slot_id));
        }
        let start = offset as usize;
        let end = start + length as usize;
        Ok(&self.data[start..end])
    }

    /// Mark a slot as deleted (zero offset and length). Does not reclaim space.
    pub fn delete_tuple(&mut self, slot_id: u16) -> Result<()> {
        if slot_id >= self.slot_count() {
            return Err(StorageError::SlotOutOfRange(slot_id));
        }
        self.write_slot(slot_id, 0, 0);
        let flags = self.flags() | FLAG_HAS_FREE_LINES;
        self.set_flags(flags);
        Ok(())
    }

    /// Insert raw bytes at a specific slot position, shifting higher slots right.
    ///
    /// Used by the B+ tree to maintain sorted order within a node page.
    pub fn insert_tuple_at(&mut self, pos: u16, data: &[u8]) -> Result<u16> {
        let needed = SLOT_SIZE + data.len();
        let available = self.free_space();
        if available < needed {
            return Err(StorageError::InsufficientSpace { needed, available });
        }
        let slot_count = self.slot_count();
        if pos > slot_count {
            return Err(StorageError::SlotOutOfRange(pos));
        }

        let new_upper = self.upper() as usize - data.len();
        self.data[new_upper..new_upper + data.len()].copy_from_slice(data);

        // Shift slots [pos..slot_count] one position right to make room
        if pos < slot_count {
            let base = PAGE_HEADER_SIZE + pos as usize * SLOT_SIZE;
            let end = PAGE_HEADER_SIZE + slot_count as usize * SLOT_SIZE;
            self.data.copy_within(base..end, base + SLOT_SIZE);
        }

        let slot_base = PAGE_HEADER_SIZE + pos as usize * SLOT_SIZE;
        write_u16(&mut self.data, slot_base, new_upper as u16);
        write_u16(&mut self.data, slot_base + 2, data.len() as u16);

        self.set_upper(new_upper as u16);
        self.set_lower(self.lower() + SLOT_SIZE as u16);

        Ok(pos)
    }

    /// Clear all tuple data and the slot array, preserving the page header and special space.
    ///
    /// Used by the B+ tree during page splits to rewrite node contents from scratch.
    pub fn clear_tuples(&mut self) {
        let sp = self.special() as usize;
        for b in &mut self.data[PAGE_HEADER_SIZE..sp] {
            *b = 0;
        }
        self.set_lower(PAGE_HEADER_SIZE as u16);
        self.set_upper(self.special());
    }

    // --- B+ tree node methods ---

    /// Initialize this page as a B+ tree node. Must be called on freshly allocated pages.
    pub fn init_btree_page(&mut self, is_leaf: bool, is_root: bool) {
        let sp = (PAGE_SIZE - BTREE_SPECIAL_SIZE) as u16;
        self.set_special(sp);
        self.set_upper(sp);
        self.set_lower(PAGE_HEADER_SIZE as u16);

        let flags: u16 = (if is_root { BTREE_FLAG_IS_ROOT } else { 0 })
            | (if is_leaf { BTREE_FLAG_IS_LEAF } else { 0 })
            | BTREE_FLAG_IS_RIGHTMOST;

        let sp_usize = sp as usize;
        write_u16(&mut self.data, sp_usize + BT_FLAGS, flags);
        write_u16(&mut self.data, sp_usize + BT_LEVEL, 0);
        write_u64(&mut self.data, sp_usize + BT_PREV, INVALID_PAGE_ID);
        write_u64(&mut self.data, sp_usize + BT_NEXT, INVALID_PAGE_ID);
        write_u64(&mut self.data, sp_usize + BT_LEFTMOST, INVALID_PAGE_ID);
    }

    pub fn btree_flags(&self) -> u16 {
        read_u16(&self.data, self.special() as usize + BT_FLAGS)
    }

    pub fn set_btree_flags(&mut self, f: u16) {
        let sp = self.special() as usize;
        write_u16(&mut self.data, sp + BT_FLAGS, f);
    }

    pub fn btree_level(&self) -> u16 {
        read_u16(&self.data, self.special() as usize + BT_LEVEL)
    }

    pub fn set_btree_level(&mut self, level: u16) {
        let sp = self.special() as usize;
        write_u16(&mut self.data, sp + BT_LEVEL, level);
    }

    pub fn btree_prev_page(&self) -> PageId {
        read_u64(&self.data, self.special() as usize + BT_PREV)
    }

    pub fn set_btree_prev_page(&mut self, pid: PageId) {
        let sp = self.special() as usize;
        write_u64(&mut self.data, sp + BT_PREV, pid);
    }

    pub fn btree_next_page(&self) -> PageId {
        read_u64(&self.data, self.special() as usize + BT_NEXT)
    }

    pub fn set_btree_next_page(&mut self, pid: PageId) {
        let sp = self.special() as usize;
        write_u64(&mut self.data, sp + BT_NEXT, pid);
    }

    pub fn btree_leftmost_child(&self) -> PageId {
        read_u64(&self.data, self.special() as usize + BT_LEFTMOST)
    }

    pub fn set_btree_leftmost_child(&mut self, pid: PageId) {
        let sp = self.special() as usize;
        write_u64(&mut self.data, sp + BT_LEFTMOST, pid);
    }

    // --- Checksum ---

    /// Compute CRC32 of the page with the checksum field zeroed.
    pub fn compute_checksum(&self) -> u32 {
        let mut data = self.data;
        // Zero out the checksum field before computing
        write_u32(&mut data, HDR_CHECKSUM, 0);
        crc32fast::hash(&data)
    }

    /// Returns true if the stored checksum matches the computed checksum.
    pub fn verify_checksum(&self) -> bool {
        self.compute_checksum() == self.checksum()
    }

    /// Recompute and store the checksum in the page header.
    pub fn update_checksum(&mut self) {
        let checksum = self.compute_checksum();
        write_u32(&mut self.data, HDR_CHECKSUM, checksum);
    }
}

// --- Byte-level helpers (little-endian) ---

#[inline]
fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap())
}

#[inline]
fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

#[inline]
fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

#[inline]
fn write_u16(data: &mut [u8], offset: usize, val: u16) {
    data[offset..offset + 2].copy_from_slice(&val.to_le_bytes());
}

#[inline]
fn write_u32(data: &mut [u8], offset: usize, val: u32) {
    data[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
}

#[inline]
fn write_u64(data: &mut [u8], offset: usize, val: u64) {
    data[offset..offset + 8].copy_from_slice(&val.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_page_initial_state() {
        let page = Page::new(42);
        assert_eq!(page.page_id(), 42);
        assert_eq!(page.lower() as usize, PAGE_HEADER_SIZE);
        assert_eq!(page.upper() as usize, PAGE_SIZE);
        assert_eq!(page.free_space(), PAGE_SIZE - PAGE_HEADER_SIZE);
        assert_eq!(page.slot_count(), 0);
        assert_eq!(page.lsn(), 0);
    }

    #[test]
    fn test_insert_and_read_single_tuple() {
        let mut page = Page::new(1);
        let data = b"hello world";
        let slot_id = page.insert_tuple(data).unwrap();
        assert_eq!(slot_id, 0);
        assert_eq!(page.get_tuple(0).unwrap(), data);
        assert_eq!(page.slot_count(), 1);
    }

    #[test]
    fn test_insert_and_read_multiple_tuples() {
        let mut page = Page::new(1);
        let tuples: Vec<Vec<u8>> = (0u8..10).map(|i| vec![i; 50]).collect();
        let mut slot_ids = Vec::new();

        for t in &tuples {
            let sid = page.insert_tuple(t).unwrap();
            slot_ids.push(sid);
        }

        for (i, sid) in slot_ids.iter().enumerate() {
            assert_eq!(page.get_tuple(*sid).unwrap(), tuples[i].as_slice());
        }
        assert_eq!(page.slot_count(), 10);
    }

    #[test]
    fn test_free_space_decreases_on_insert() {
        let mut page = Page::new(1);
        let initial = page.free_space();
        let data = vec![0u8; 100];
        page.insert_tuple(&data).unwrap();
        // Used: SLOT_SIZE (4) + 100 = 104 bytes
        assert_eq!(page.free_space(), initial - 104);
    }

    #[test]
    fn test_insert_fills_page_returns_error() {
        let mut page = Page::new(1);
        // Fill the page with tuples until no more fit
        let data = vec![0u8; 200];
        let mut count = 0;
        loop {
            match page.insert_tuple(&data) {
                Ok(_) => count += 1,
                Err(StorageError::InsufficientSpace { .. }) => break,
                Err(e) => panic!("unexpected error: {}", e),
            }
        }
        // 8192 - 48 = 8144 free. Each tuple uses 204 bytes. 8144 / 204 = 39 tuples.
        assert!(count > 0);
        assert!(page.free_space() < 204);
    }

    #[test]
    fn test_delete_tuple_marks_slot_unused() {
        let mut page = Page::new(1);
        page.insert_tuple(b"data").unwrap();
        assert!(page.is_slot_used(0));
        page.delete_tuple(0).unwrap();
        assert!(!page.is_slot_used(0));
        assert!(matches!(page.get_tuple(0), Err(StorageError::SlotOutOfRange(0))));
    }

    #[test]
    fn test_delete_sets_has_free_lines_flag() {
        let mut page = Page::new(1);
        page.insert_tuple(b"x").unwrap();
        page.delete_tuple(0).unwrap();
        assert!(page.flags() & FLAG_HAS_FREE_LINES != 0);
    }

    #[test]
    fn test_slot_out_of_range() {
        let page = Page::new(1);
        assert!(matches!(page.get_tuple(0), Err(StorageError::SlotOutOfRange(0))));
        assert!(matches!(page.get_tuple(99), Err(StorageError::SlotOutOfRange(99))));
    }

    #[test]
    fn test_checksum_round_trip() {
        let mut page = Page::new(7);
        page.insert_tuple(b"checksum test data").unwrap();
        page.update_checksum();
        assert!(page.verify_checksum());
    }

    #[test]
    fn test_checksum_fails_on_corruption() {
        let mut page = Page::new(7);
        page.insert_tuple(b"data").unwrap();
        page.update_checksum();
        // Corrupt a byte in the tuple data area
        page.as_bytes_mut()[PAGE_SIZE - 10] ^= 0xFF;
        assert!(!page.verify_checksum());
    }

    #[test]
    fn test_from_bytes_round_trip() {
        let mut page = Page::new(99);
        page.insert_tuple(b"round trip").unwrap();
        page.update_checksum();

        let bytes = *page.as_bytes();
        let page2 = Page::from_bytes(bytes);
        assert_eq!(page2.page_id(), 99);
        assert_eq!(page2.get_tuple(0).unwrap(), b"round trip");
        assert!(page2.verify_checksum());
    }

    #[test]
    fn test_lsn_set_get() {
        let mut page = Page::new(1);
        page.set_lsn(12345678);
        assert_eq!(page.lsn(), 12345678);
    }
}
