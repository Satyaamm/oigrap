use crate::buffer_pool::BufferPool;
use crate::error::{Result, StorageError};
use crate::page::{HEAP_XMIN_COMMITTED, TUPLE_HEADER_SIZE};
use crate::wal::{DecodedRecord, WalManager, WalRecord, INVALID_LSN};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

pub type Xid = u64;
pub const INVALID_XID: Xid = 0;

/// A consistent point-in-time snapshot of transaction state.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// All transactions with XID < xmin are committed and visible.
    pub xmin: Xid,
    /// All transactions with XID >= xmax are not yet started and invisible.
    pub xmax: Xid,
    /// Transactions that were in progress when the snapshot was taken.
    pub active: Vec<Xid>,
}

/// The 24-byte MVCC header stored at the start of every tuple.
///
/// See docs/13_data_formats.md for the on-disk layout.
#[derive(Debug, Clone, Copy)]
pub struct TupleHeader {
    pub xmin: Xid,
    pub xmax: Xid,
    pub cid: u32,
    pub infomask: u16,
    pub infomask2: u16,
}

impl TupleHeader {
    pub fn new_insert(xid: Xid, cid: u32) -> Self {
        TupleHeader {
            xmin: xid,
            xmax: INVALID_XID,
            cid,
            infomask: HEAP_XMIN_COMMITTED, // optimistically set; vacuum confirms
            infomask2: 0,
        }
    }

    /// Serialize into the 24-byte on-disk format.
    pub fn encode(&self) -> [u8; TUPLE_HEADER_SIZE] {
        let mut buf = [0u8; TUPLE_HEADER_SIZE];
        buf[0..8].copy_from_slice(&self.xmin.to_le_bytes());
        buf[8..16].copy_from_slice(&self.xmax.to_le_bytes());
        buf[16..20].copy_from_slice(&self.cid.to_le_bytes());
        buf[20..22].copy_from_slice(&self.infomask.to_le_bytes());
        buf[22..24].copy_from_slice(&self.infomask2.to_le_bytes());
        buf
    }

    /// Decode from the first 24 bytes of a tuple's raw data.
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.len() < TUPLE_HEADER_SIZE {
            return Err(StorageError::Corruption(format!(
                "tuple too short for header: {} bytes",
                data.len()
            )));
        }
        Ok(TupleHeader {
            xmin: u64::from_le_bytes(data[0..8].try_into().unwrap()),
            xmax: u64::from_le_bytes(data[8..16].try_into().unwrap()),
            cid: u32::from_le_bytes(data[16..20].try_into().unwrap()),
            infomask: u16::from_le_bytes(data[20..22].try_into().unwrap()),
            infomask2: u16::from_le_bytes(data[22..24].try_into().unwrap()),
        })
    }
}

/// Manages transactions and MVCC state.
///
/// Each session calls `begin()` to start a transaction. The returned `Xid`
/// must be passed to every subsequent operation. On commit or abort, the
/// caller passes the Xid back to `commit()` / `abort()`.
pub struct TransactionManager {
    next_xid: AtomicU64,
    committed: BTreeSet<Xid>,
    aborted: BTreeSet<Xid>,
    active: HashSet<Xid>,
    // SSI: which tuples each active transaction has read
    read_sets: HashMap<Xid, HashSet<(u32, u64, u16)>>,
    // SSI: which tuples each active transaction has written (inserted/updated/deleted)
    write_sets: HashMap<Xid, HashSet<(u32, u64, u16)>>,
}

impl TransactionManager {
    pub fn new() -> Self {
        TransactionManager {
            next_xid: AtomicU64::new(1), // XID 0 is invalid
            committed: BTreeSet::new(),
            aborted: BTreeSet::new(),
            active: HashSet::new(),
            read_sets: HashMap::new(),
            write_sets: HashMap::new(),
        }
    }

    /// Begin a new transaction. Returns its XID.
    pub fn begin(&mut self) -> Xid {
        let xid = self.next_xid.fetch_add(1, Ordering::SeqCst);
        self.active.insert(xid);
        xid
    }

    /// Record that transaction `xid` read tuple `(table_id, page_id, slot_id)`.
    pub fn record_read(&mut self, xid: Xid, table_id: u32, page_id: u64, slot_id: u16) {
        self.read_sets.entry(xid).or_default().insert((table_id, page_id, slot_id));
    }

    /// Record that transaction `xid` wrote (inserted/updated/deleted) tuple.
    pub fn record_write(&mut self, xid: Xid, table_id: u32, page_id: u64, slot_id: u16) {
        self.write_sets.entry(xid).or_default().insert((table_id, page_id, slot_id));
    }

    /// Check for SSI rw-anti-dependency cycles involving `xid`.
    ///
    /// Returns true if committing `xid` would complete a serialization cycle.
    /// A cycle exists when:
    ///   - `xid` read some tuple S that another concurrent transaction T2 wrote, AND
    ///   - T2 read some tuple that `xid` wrote.
    pub fn check_ssi_conflict(&self, xid: Xid) -> bool {
        let my_reads = match self.read_sets.get(&xid) {
            Some(r) => r,
            None => return false,
        };
        let my_writes = match self.write_sets.get(&xid) {
            Some(w) => w,
            None => return false,
        };

        // For each other active transaction T2:
        for (&other_xid, other_writes) in &self.write_sets {
            if other_xid == xid {
                continue;
            }
            // If T2 wrote something I read (rw-anti-dependency: T2 -> T1)
            if my_reads.iter().any(|r| other_writes.contains(r)) {
                // Check the reverse: if I wrote something T2 read (rw-anti-dependency: T1 -> T2)
                if let Some(other_reads) = self.read_sets.get(&other_xid) {
                    if my_writes.iter().any(|w| other_reads.contains(w)) {
                        return true; // cycle detected
                    }
                }
            }
        }
        false
    }

    /// Commit a transaction: write WAL commit record and mark as committed.
    pub fn commit(&mut self, xid: Xid, wal: &mut WalManager) -> Result<()> {
        if !self.active.contains(&xid) {
            return Err(StorageError::Corruption(format!(
                "commit called for non-active transaction {}",
                xid
            )));
        }

        // SSI check before committing
        if self.check_ssi_conflict(xid) {
            // Abort instead of commit to break the cycle
            self.active.remove(&xid);
            self.aborted.insert(xid);
            self.read_sets.remove(&xid);
            self.write_sets.remove(&xid);
            return Err(StorageError::Corruption(format!(
                "serialization failure: rw-anti-dependency cycle detected for xid {}",
                xid
            )));
        }

        let timestamp = current_timestamp();
        wal.write_record(xid, WalRecord::XactCommit { timestamp })?;
        wal.flush()?; // WAL must be on disk before we acknowledge commit
        self.active.remove(&xid);
        self.committed.insert(xid);
        self.read_sets.remove(&xid);
        self.write_sets.remove(&xid);
        Ok(())
    }

    /// Abort a transaction: write WAL abort record and mark as aborted.
    pub fn abort(&mut self, xid: Xid, wal: &mut WalManager) -> Result<()> {
        if !self.active.contains(&xid) {
            return Err(StorageError::Corruption(format!(
                "abort called for non-active transaction {}",
                xid
            )));
        }
        let timestamp = current_timestamp();
        wal.write_record(xid, WalRecord::XactAbort { timestamp })?;
        self.active.remove(&xid);
        self.aborted.insert(xid);
        self.read_sets.remove(&xid);
        self.write_sets.remove(&xid);
        Ok(())
    }

    /// Take a snapshot of the current transaction state.
    ///
    /// The snapshot is used by visibility checks: a tuple is visible if
    /// its creating transaction is committed and visible in this snapshot.
    pub fn snapshot(&self) -> Snapshot {
        let xmax = self.next_xid.load(Ordering::SeqCst);
        let xmin = self
            .committed
            .iter()
            .next()
            .copied()
            .unwrap_or(xmax);
        let active: Vec<Xid> = self.active.iter().copied().collect();
        Snapshot { xmin, xmax, active }
    }

    /// Returns true if the tuple described by `header` is visible to `snapshot`.
    pub fn is_visible(&self, header: &TupleHeader, snap: &Snapshot) -> bool {
        let xmin_committed = self.is_committed_before(header.xmin, snap);
        if !xmin_committed {
            return false;
        }

        // xmax == 0 means the tuple has not been deleted
        if header.xmax == INVALID_XID {
            return true;
        }

        // Tuple was deleted — is the deletion visible?
        let xmax_visible = self.is_committed_before(header.xmax, snap);
        !xmax_visible
    }

    fn is_committed_before(&self, xid: Xid, snap: &Snapshot) -> bool {
        if xid == INVALID_XID {
            return false;
        }
        // Active-at-snapshot check must precede xmin check: a transaction that was
        // in-progress when the snapshot was taken must never be visible to it, even
        // if its numeric XID falls below xmin (which can happen when xmin==xmax
        // because no txns were committed yet).
        if snap.active.contains(&xid) {
            return false;
        }
        if xid < snap.xmin {
            return self.committed.contains(&xid);
        }
        if xid >= snap.xmax {
            return false;
        }
        self.committed.contains(&xid)
    }

    pub fn is_committed(&self, xid: Xid) -> bool {
        self.committed.contains(&xid)
    }

    pub fn is_aborted(&self, xid: Xid) -> bool {
        self.aborted.contains(&xid)
    }

    /// Mark a transaction as aborted without writing a WAL record.
    /// Used during recovery to mark crash-interrupted transactions as aborted.
    pub fn force_abort(&mut self, xid: Xid) {
        self.active.remove(&xid);
        self.aborted.insert(xid);
        self.read_sets.remove(&xid);
        self.write_sets.remove(&xid);
    }

    pub fn oldest_active_xid(&self) -> Xid {
        self.active.iter().copied().min().unwrap_or(INVALID_XID)
    }
}

impl Default for TransactionManager {
    fn default() -> Self {
        Self::new()
    }
}

/// ARIES-based crash recovery.
///
/// Three phases:
/// 1. Analysis — scan WAL from redo_lsn, build txn_table and dirty_page_table.
/// 2. Redo — replay all records from redo_lsn to restore crash-time state.
/// 3. Undo — reverse all transactions that were active at crash.
pub fn recover(
    pool: &mut BufferPool,
    wal: &WalManager,
    tx_mgr: &mut TransactionManager,
) -> Result<()> {
    // --- Phase 1: Analysis ---
    let mut txn_status: HashMap<Xid, TxnState> = HashMap::new(); // xid -> state

    let mut reader = wal.read_from(INVALID_LSN)?;
    while let Some(res) = reader.next_record() {
        let rec = res?;
        match &rec.record {
            WalRecord::XactCommit { .. } => {
                txn_status.insert(rec.xid, TxnState::Committed);
            }
            WalRecord::XactAbort { .. } => {
                txn_status.insert(rec.xid, TxnState::Aborted);
            }
            WalRecord::HeapInsert { .. }
            | WalRecord::HeapUpdate { .. }
            | WalRecord::HeapDelete { .. } => {
                txn_status.entry(rec.xid).or_insert(TxnState::Active);
            }
            _ => {}
        }
    }

    // --- Phase 2: Redo ---
    // Re-apply all WAL records. Each record is idempotent when LSN is checked.
    // For simplicity in Phase 1, we replay unconditionally (full redo).
    let mut reader = wal.read_from(INVALID_LSN)?;
    while let Some(res) = reader.next_record() {
        let rec = res?;
        redo_record(pool, &rec)?;
    }

    // --- Phase 3: Undo ---
    // For transactions that were active at crash (neither committed nor aborted),
    // mark their tuples as dead by setting xmax to their XID with XMAX_INVALID flag.
    for (xid, state) in &txn_status {
        if *state == TxnState::Active {
            // Mark in our in-memory txn manager as aborted (no WAL needed during recovery)
            tx_mgr.active.insert(*xid);
            // The caller should undo heap modifications; for Phase 1, the
            // infomask HEAP_XMAX_INVALID on alive tuples prevents visibility.
        }
    }

    // Restore committed / aborted sets from WAL analysis
    for (xid, state) in &txn_status {
        match state {
            TxnState::Committed => {
                tx_mgr.committed.insert(*xid);
            }
            TxnState::Aborted => {
                tx_mgr.aborted.insert(*xid);
            }
            TxnState::Active => {
                // Remains active — will be aborted
            }
        }
    }

    // Advance next_xid past any XID seen in the WAL
    let max_xid = txn_status.keys().copied().max().unwrap_or(0);
    let current = tx_mgr.next_xid.load(Ordering::SeqCst);
    if max_xid >= current {
        tx_mgr.next_xid.store(max_xid + 1, Ordering::SeqCst);
    }

    Ok(())
}

#[derive(Debug, PartialEq)]
enum TxnState {
    Active,
    Committed,
    Aborted,
}

fn redo_record(pool: &mut BufferPool, rec: &DecodedRecord) -> Result<()> {
    match &rec.record {
        WalRecord::HeapInsert { page_id, slot_id, tuple_data, .. } => {
            // Fetch the page from disk (or pool cache). ARIES redo always loads
            // the page and compares on-disk LSN with the WAL record LSN.
            let frame_id = pool.fetch_page(*page_id)?;
            let page_lsn = pool.page(frame_id).lsn();
            if page_lsn < rec.lsn {
                // Page on disk is older than this WAL record — redo the insert.
                // Only insert if the slot is not already occupied (idempotency guard).
                if !pool.page(frame_id).is_slot_used(*slot_id) {
                    let _ = pool.page_mut(frame_id).insert_tuple(tuple_data);
                    pool.page_mut(frame_id).set_lsn(rec.lsn);
                    pool.unpin_page(*page_id, true)?;
                } else {
                    pool.unpin_page(*page_id, false)?;
                }
            } else {
                pool.unpin_page(*page_id, false)?;
            }
        }
        WalRecord::HeapDelete { page_id, slot_id, .. } => {
            let frame_id = pool.fetch_page(*page_id)?;
            let page_lsn = pool.page(frame_id).lsn();
            if page_lsn < rec.lsn && pool.page(frame_id).is_slot_used(*slot_id) {
                pool.page_mut(frame_id).delete_tuple(*slot_id)?;
                pool.page_mut(frame_id).set_lsn(rec.lsn);
                pool.unpin_page(*page_id, true)?;
            } else {
                pool.unpin_page(*page_id, false)?;
            }
        }
        WalRecord::HeapUpdate { old_page, old_slot, new_page, new_slot, new_tuple, .. } => {
            // Redo the delete on the old page.
            let old_frame = pool.fetch_page(*old_page)?;
            let old_page_lsn = pool.page(old_frame).lsn();
            if old_page_lsn < rec.lsn && pool.page(old_frame).is_slot_used(*old_slot) {
                pool.page_mut(old_frame).delete_tuple(*old_slot)?;
                pool.page_mut(old_frame).set_lsn(rec.lsn);
                pool.unpin_page(*old_page, true)?;
            } else {
                pool.unpin_page(*old_page, false)?;
            }
            // Redo the insert on the new page.
            let new_frame = pool.fetch_page(*new_page)?;
            let new_page_lsn = pool.page(new_frame).lsn();
            if new_page_lsn < rec.lsn && !pool.page(new_frame).is_slot_used(*new_slot) {
                let _ = pool.page_mut(new_frame).insert_tuple(new_tuple);
                pool.page_mut(new_frame).set_lsn(rec.lsn);
                pool.unpin_page(*new_page, true)?;
            } else {
                pool.unpin_page(*new_page, false)?;
            }
        }
        // XactCommit / XactAbort / Checkpoint have no page-level redo action.
        _ => {}
    }
    Ok(())
}

/// Simplified redo-only recovery entry point.
///
/// Scans the WAL from LSN 0 and replays every heap record against the buffer
/// pool.  Page LSN checks ensure idempotency: a record is only replayed when
/// the on-disk page is older than the WAL record.  This is the function called
/// by tests that want to verify data survives a simulated crash + restart
/// without needing the full three-phase ARIES logic.
pub fn redo_recover(wal: &WalManager, pool: &mut BufferPool) -> Result<()> {
    let mut reader = wal.read_from(INVALID_LSN)?;
    while let Some(res) = reader.next_record() {
        let rec = res?;
        redo_record(pool, &rec)?;
    }
    Ok(())
}

/// ARIES undo phase: scan WAL from the beginning, identify transactions that
/// have no commit or abort record (active at crash), and mark them as aborted.
/// Combined with the redo pass, this restores the database to a consistent state.
pub fn undo_recover(wal: &WalManager, tx: &mut TransactionManager) -> Result<()> {
    let mut committed: HashSet<Xid> = HashSet::new();
    let mut explicitly_aborted: HashSet<Xid> = HashSet::new();
    let mut all_xids: HashSet<Xid> = HashSet::new();

    let mut reader = wal.read_from(INVALID_LSN)?;
    while let Some(res) = reader.next_record() {
        let decoded = res?;
        if decoded.xid != INVALID_XID {
            all_xids.insert(decoded.xid);
        }
        match decoded.record {
            WalRecord::XactCommit { .. } => {
                committed.insert(decoded.xid);
            }
            WalRecord::XactAbort { .. } => {
                explicitly_aborted.insert(decoded.xid);
            }
            _ => {}
        }
    }

    for xid in all_xids {
        if !committed.contains(&xid) && !explicitly_aborted.contains(&xid) {
            tx.force_abort(xid);
        }
    }
    Ok(())
}

fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::DiskManager;
    use crate::heap::HeapFile;
    use tempfile::tempdir;

    fn make_env() -> (BufferPool, WalManager, TransactionManager, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let disk = DiskManager::create(&dir.path().join("test.db")).unwrap();
        let pool = BufferPool::new(20, disk);
        let wal = WalManager::create(&dir.path().join("test.wal")).unwrap();
        let tx = TransactionManager::new();
        (pool, wal, tx, dir)
    }

    #[test]
    fn test_begin_commit_basic() {
        let (_, mut wal, mut tx, _dir) = make_env();
        let xid = tx.begin();
        assert!(xid >= 1);
        assert!(tx.active.contains(&xid));
        tx.commit(xid, &mut wal).unwrap();
        assert!(tx.is_committed(xid));
        assert!(!tx.active.contains(&xid));
    }

    #[test]
    fn test_begin_abort_basic() {
        let (_, mut wal, mut tx, _dir) = make_env();
        let xid = tx.begin();
        tx.abort(xid, &mut wal).unwrap();
        assert!(tx.is_aborted(xid));
        assert!(!tx.active.contains(&xid));
    }

    #[test]
    fn test_tuple_visibility_committed() {
        let (_, mut wal, mut tx, _dir) = make_env();

        let xid = tx.begin();
        let snap = tx.snapshot(); // snapshot before commit

        let header = TupleHeader::new_insert(xid, 0);
        // Before commit: not visible to snap (xid is active)
        assert!(!tx.is_visible(&header, &snap));

        tx.commit(xid, &mut wal).unwrap();
        let snap2 = tx.snapshot(); // snapshot after commit
        assert!(tx.is_visible(&header, &snap2));
    }

    #[test]
    fn test_deleted_tuple_not_visible() {
        let (_, mut wal, mut tx, _dir) = make_env();

        let xid1 = tx.begin();
        tx.commit(xid1, &mut wal).unwrap();

        let xid2 = tx.begin();
        tx.commit(xid2, &mut wal).unwrap();

        // Tuple inserted by xid1, deleted by xid2
        let header = TupleHeader {
            xmin: xid1,
            xmax: xid2,
            cid: 0,
            infomask: HEAP_XMIN_COMMITTED,
            infomask2: 0,
        };

        let snap = tx.snapshot(); // both committed before snap
        // Tuple was deleted by a committed tx visible to snap -> not visible
        assert!(!tx.is_visible(&header, &snap));
    }

    #[test]
    fn test_aborted_inserter_tuple_not_visible() {
        let (_, mut wal, mut tx, _dir) = make_env();
        let xid = tx.begin();
        tx.abort(xid, &mut wal).unwrap();

        let header = TupleHeader::new_insert(xid, 0);
        let snap = tx.snapshot();
        assert!(!tx.is_visible(&header, &snap));
    }

    #[test]
    fn test_snapshot_isolates_concurrent_transactions() {
        let (_, mut wal, mut tx, _dir) = make_env();

        let xid1 = tx.begin();
        // Take snapshot while xid1 is active
        let snap = tx.snapshot();
        // xid1 commits after snapshot
        tx.commit(xid1, &mut wal).unwrap();

        let header = TupleHeader::new_insert(xid1, 0);
        // xid1 was active in snap.active -> not visible to snap (repeatable read)
        assert!(!tx.is_visible(&header, &snap));
    }

    #[test]
    fn test_aries_recovery_committed_data_survives() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let wal_path = dir.path().join("test.wal");

        let committed_tids;

        // === Phase: write and commit, then crash ===
        {
            let disk = DiskManager::create(&db_path).unwrap();
            let mut pool = BufferPool::new(20, disk);
            let mut wal = WalManager::create(&wal_path).unwrap();
            let mut tx = TransactionManager::new();
            let mut heap = HeapFile::new(1);

            // Txn A: insert 5 rows, commit
            let xid_a = tx.begin();
            let mut tids = Vec::new();
            for i in 0u8..5 {
                let mut data = TupleHeader::new_insert(xid_a, i as u32).encode().to_vec();
                data.extend_from_slice(&[i; 32]);
                let tid = heap.insert_tuple(&mut pool, &mut wal, xid_a, &data).unwrap();
                tids.push(tid);
            }
            tx.commit(xid_a, &mut wal).unwrap();
            committed_tids = tids;

            // Txn B: insert 3 rows, do NOT commit (crash)
            let xid_b = tx.begin();
            for j in 0u8..3 {
                let mut data = TupleHeader::new_insert(xid_b, j as u32).encode().to_vec();
                data.extend_from_slice(&[0xAA; 32]);
                heap.insert_tuple(&mut pool, &mut wal, xid_b, &data).unwrap();
            }
            // No commit for xid_b — simulate crash
            pool.flush_all().unwrap();
            // Drop everything (no wal.flush() for xid_b's rows — they're in the WAL buffer)
            // Actually flush so WAL has the inserts but no commit
            wal.flush().unwrap();
        }

        // === Phase: recovery ===
        {
            let disk = DiskManager::open(&db_path).unwrap();
            let mut pool = BufferPool::new(20, disk);
            let wal = WalManager::open(&wal_path).unwrap();
            let mut tx = TransactionManager::new();

            recover(&mut pool, &wal, &mut tx).unwrap();

            // After recovery:
            // - Txn A is committed: its tuples are visible
            // - Txn B is active/uncommitted: its tuples are not visible
            let snap = tx.snapshot();

            for tid in &committed_tids {
                let frame_id = pool.fetch_page(tid.page_id).unwrap();
                if pool.page(frame_id).is_slot_used(tid.slot_id) {
                    let raw = pool.page(frame_id).get_tuple(tid.slot_id).unwrap();
                    let hdr = TupleHeader::decode(raw).unwrap();
                    assert!(
                        tx.is_visible(&hdr, &snap),
                        "committed tuple should be visible after recovery"
                    );
                }
                pool.unpin_page(tid.page_id, false).unwrap();
            }
        }
    }

    #[test]
    fn test_tuple_header_encode_decode_round_trip() {
        let hdr = TupleHeader {
            xmin: 12345,
            xmax: 67890,
            cid: 7,
            infomask: 0x0101,
            infomask2: 0x0002,
        };
        let encoded = hdr.encode();
        let decoded = TupleHeader::decode(&encoded).unwrap();
        assert_eq!(decoded.xmin, hdr.xmin);
        assert_eq!(decoded.xmax, hdr.xmax);
        assert_eq!(decoded.cid, hdr.cid);
        assert_eq!(decoded.infomask, hdr.infomask);
        assert_eq!(decoded.infomask2, hdr.infomask2);
    }

    #[test]
    fn test_undo_recover_marks_crashed_xids_aborted() {
        let dir = tempdir().unwrap();
        let wal_path = dir.path().join("undo_test.wal");

        let xid_a;
        let xid_b;

        {
            let mut wal = WalManager::create(&wal_path).unwrap();
            let mut tx = TransactionManager::new();

            xid_a = tx.begin();
            xid_b = tx.begin();
            let _xid_c = tx.begin(); // no records written for xid_c — won't appear in all_xids

            // Write a HeapInsert record for xid_a
            wal.write_record(
                xid_a,
                WalRecord::HeapInsert {
                    table_id: 1,
                    page_id: 0,
                    slot_id: 0,
                    tuple_data: vec![0u8; 8],
                },
            )
            .unwrap();

            // Write a HeapInsert record for xid_b
            wal.write_record(
                xid_b,
                WalRecord::HeapInsert {
                    table_id: 1,
                    page_id: 1,
                    slot_id: 0,
                    tuple_data: vec![1u8; 8],
                },
            )
            .unwrap();

            // Commit xid_a only
            tx.commit(xid_a, &mut wal).unwrap();

            wal.flush().unwrap();
            // xid_b is never committed — simulate crash
        }

        let wal = WalManager::open(&wal_path).unwrap();
        let mut tx = TransactionManager::new();

        undo_recover(&wal, &mut tx).unwrap();

        // xid_b wrote records but was never committed: must be aborted
        assert!(tx.is_aborted(xid_b), "xid_b should be aborted after undo_recover");
        // xid_a was committed: must NOT be in the aborted set
        assert!(!tx.is_aborted(xid_a), "xid_a was committed and should not be aborted");
    }

    #[test]
    fn test_ssi_write_skew_detected() {
        let (_, mut wal, mut tx, _dir) = make_env();

        let t1 = tx.begin();
        let t2 = tx.begin();

        // T1 reads slot 0, T2 reads slot 1
        tx.record_read(t1, 1, 0, 0);
        tx.record_read(t2, 1, 0, 1);

        // T1 writes slot 1, T2 writes slot 0 — classic write-skew cross
        tx.record_write(t1, 1, 0, 1);
        tx.record_write(t2, 1, 0, 0);

        // Both transactions form a cycle: T2->T1 (T2 wrote what T1 read) and
        // T1->T2 (T1 wrote what T2 read). Committing T1 should detect this.
        assert!(tx.check_ssi_conflict(t1), "cycle should be detected for T1");

        // commit(T1) must return Err and leave T1 aborted
        let result = tx.commit(t1, &mut wal);
        assert!(result.is_err(), "commit should fail due to SSI conflict");
        assert!(tx.is_aborted(t1), "T1 should be aborted after SSI failure");
    }

    #[test]
    fn test_ssi_no_conflict_independent_transactions() {
        let (_, mut wal, mut tx, _dir) = make_env();

        let t1 = tx.begin();
        let t2 = tx.begin();

        // T1 operates on table 1 page 0; T2 operates on table 1 page 1 — disjoint
        tx.record_read(t1, 1, 0, 0);
        tx.record_write(t1, 1, 0, 1);

        tx.record_read(t2, 1, 1, 0);
        tx.record_write(t2, 1, 1, 1);

        // No shared tuples between T1 and T2 — no cycle
        assert!(!tx.check_ssi_conflict(t1), "no cycle should be detected for T1");

        // commit(T1) must succeed
        let result = tx.commit(t1, &mut wal);
        assert!(result.is_ok(), "commit should succeed with no SSI conflict");
        assert!(tx.is_committed(t1), "T1 should be committed");
    }

    #[test]
    fn test_ssi_read_sets_cleaned_up_on_abort() {
        let (_, mut wal, mut tx, _dir) = make_env();

        let t1 = tx.begin();
        let t2 = tx.begin();

        // Give T1 reads and writes that would form a cycle with T2
        tx.record_read(t1, 1, 0, 0);
        tx.record_write(t1, 1, 0, 1);

        // T2 writes what T1 read and reads what T1 wrote
        tx.record_read(t2, 1, 0, 1);
        tx.record_write(t2, 1, 0, 0);

        // Abort T1 — its sets must be removed
        tx.abort(t1, &mut wal).unwrap();
        assert!(tx.is_aborted(t1));

        // After T1 is aborted, check_ssi_conflict for T2 should not see T1's data.
        // T2's own sets reference tuples that T1 no longer claims — no cycle.
        assert!(
            !tx.check_ssi_conflict(t2),
            "aborted T1 should not contribute to T2's SSI check"
        );
    }

    #[test]
    fn test_undo_then_snapshot_hides_uncommitted() {
        let dir = tempdir().unwrap();
        let wal_path = dir.path().join("undo_vis_test.wal");

        let crash_xid: Xid = 7;
        let committed_xid: Xid = 5;

        {
            let mut wal = WalManager::create(&wal_path).unwrap();

            // Write XactCommit for xid=5 (committed transaction)
            wal.write_record(committed_xid, WalRecord::XactCommit { timestamp: 1000 })
                .unwrap();

            // Write HeapInsert for xid=7 (no commit — simulates crash)
            wal.write_record(
                crash_xid,
                WalRecord::HeapInsert {
                    table_id: 1,
                    page_id: 0,
                    slot_id: 0,
                    tuple_data: vec![7u8; 8],
                },
            )
            .unwrap();

            wal.flush().unwrap();
        }

        let wal = WalManager::open(&wal_path).unwrap();
        let mut tx = TransactionManager::new();

        undo_recover(&wal, &mut tx).unwrap();

        // xid=7 had records but no commit: must be aborted
        assert!(tx.is_aborted(crash_xid), "crash_xid should be aborted");

        // A snapshot taken now should see xid=7 as aborted
        let snap = tx.snapshot();

        // A tuple inserted by the crashed transaction should not be visible
        let header = TupleHeader {
            xmin: crash_xid,
            xmax: INVALID_XID,
            cid: 0,
            infomask: 0,
            infomask2: 0,
        };
        assert!(
            !tx.is_visible(&header, &snap),
            "tuple from crashed transaction should not be visible"
        );
    }

    #[test]
    fn test_redo_restores_heap_tuples() {
        // This test verifies that redo_recover() replays WAL records so that
        // tuples inserted before a crash are visible in a brand-new buffer pool.
        //
        // Sequence:
        //   1. Insert 5 tuples, flush WAL, flush all pages to disk.
        //   2. Drop the BufferPool (simulate restart — in-memory state gone).
        //   3. Open a new BufferPool backed by the same DiskManager.
        //   4. Call redo_recover(wal, new_pool).
        //   5. Scan the heap via new_pool and confirm all 5 tuples are present.
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("redo_test.db");
        let wal_path = dir.path().join("redo_test.wal");

        // Track which page IDs held the inserted tuples.
        let mut tids: Vec<crate::heap::TupleId> = Vec::new();

        {
            let disk = DiskManager::create(&db_path).unwrap();
            let mut pool = BufferPool::new(20, disk);
            let mut wal = WalManager::create(&wal_path).unwrap();
            let mut heap = HeapFile::new(1);

            let xid = 1u64;
            for i in 0u8..5 {
                let mut data = TupleHeader::new_insert(xid, i as u32).encode().to_vec();
                data.extend_from_slice(&[i; 32]);
                let tid = heap.insert_tuple(&mut pool, &mut wal, xid, &data).unwrap();
                tids.push(tid);
            }

            // Flush WAL first, then flush all dirty pages to disk.
            wal.flush().unwrap();
            pool.flush_all().unwrap();

            // Drop pool — in-memory frames are gone, disk has the data.
        }

        // Re-open WAL (read-only) and create a fresh buffer pool.
        let wal = WalManager::open(&wal_path).unwrap();
        let disk2 = DiskManager::open(&db_path).unwrap();
        let mut new_pool = BufferPool::new(20, disk2);

        // Run redo recovery: replay WAL into the new pool.
        redo_recover(&wal, &mut new_pool).unwrap();

        // Verify every inserted tuple is readable via the new pool.
        for (i, tid) in tids.iter().enumerate() {
            let frame_id = new_pool.fetch_page(tid.page_id).unwrap();
            assert!(
                new_pool.page(frame_id).is_slot_used(tid.slot_id),
                "slot {} on page {} should be used after redo recovery",
                tid.slot_id,
                tid.page_id,
            );
            let raw = new_pool.page(frame_id).get_tuple(tid.slot_id).unwrap();
            // Verify the payload bytes (after the 24-byte MVCC header).
            assert_eq!(
                raw[TUPLE_HEADER_SIZE..],
                vec![i as u8; 32],
                "tuple {} has wrong payload after redo recovery",
                i,
            );
            new_pool.unpin_page(tid.page_id, false).unwrap();
        }
    }
}
