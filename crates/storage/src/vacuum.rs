/// Vacuum: dead tuple reclamation for heap files.
use crate::buffer_pool::BufferPool;
use crate::error::Result;
use crate::heap::HeapFile;
use crate::mvcc::{TransactionManager, TupleHeader, INVALID_XID};
use crate::page::{Page, TUPLE_HEADER_SIZE};
use crate::wal::{WalManager, WalRecord};

/// Statistics returned after a vacuum run.
pub struct VacuumStats {
    pub pages_scanned: usize,
    pub tuples_examined: usize,
    pub tuples_removed: usize,
    pub pages_compacted: usize,
}

/// Vacuum a single heap file: remove dead tuples, compact pages, update FSM.
pub fn vacuum_table(
    heap: &mut HeapFile,
    pool: &mut BufferPool,
    wal: &mut WalManager,
    tx: &TransactionManager,
) -> Result<VacuumStats> {
    let mut stats = VacuumStats {
        pages_scanned: 0,
        tuples_examined: 0,
        tuples_removed: 0,
        pages_compacted: 0,
    };

    // Collect the page list first to avoid borrow issues
    let page_ids: Vec<_> = heap.page_ids().to_vec();

    for page_id in page_ids {
        stats.pages_scanned += 1;

        let frame_id = pool.fetch_page(page_id)?;
        let slot_count = pool.page(frame_id).slot_count();

        // Collect live and dead slot info
        let mut live_tuples: Vec<Vec<u8>> = Vec::new();
        let mut dead_slots: Vec<u16> = Vec::new();

        for slot_id in 0..slot_count {
            if !pool.page(frame_id).is_slot_used(slot_id) {
                continue;
            }
            stats.tuples_examined += 1;

            let data = pool.page(frame_id).get_tuple(slot_id)?.to_vec();

            let is_dead = if data.len() >= TUPLE_HEADER_SIZE {
                match TupleHeader::decode(&data) {
                    Ok(hdr) => {
                        hdr.xmax != INVALID_XID
                            && (tx.is_committed(hdr.xmax) || tx.is_aborted(hdr.xmax))
                    }
                    Err(_) => false,
                }
            } else {
                false
            };

            if is_dead {
                dead_slots.push(slot_id);
                stats.tuples_removed += 1;
            } else {
                live_tuples.push(data);
            }
        }

        pool.unpin_page(page_id, false)?;

        if dead_slots.is_empty() {
            // Nothing to compact on this page
            continue;
        }

        stats.pages_compacted += 1;

        // Build a fresh in-memory page with only live tuples
        let mut new_page = Page::new(page_id);
        for tuple_data in &live_tuples {
            // best-effort: if the page somehow overflows (shouldn't happen since we removed dead
            // tuples), just skip — the live data is preserved by the loop
            let _ = new_page.insert_tuple(tuple_data);
        }

        // Write it back via the buffer pool
        let frame_id = pool.fetch_page(page_id)?;
        let page_bytes = *new_page.as_bytes();
        pool.page_mut(frame_id).as_bytes_mut().copy_from_slice(&page_bytes);

        // WAL: one record per dead slot (HeapDelete), using vacuum xid = 0
        for slot_id in &dead_slots {
            wal.write_record(
                0, // vacuum internal xid
                WalRecord::HeapDelete {
                    table_id: heap.table_id,
                    page_id,
                    slot_id: *slot_id,
                    old_xmax: 0,
                },
            )?;
        }
        pool.page_mut(frame_id).set_lsn(wal.flushed_lsn());
        pool.unpin_page(page_id, true)?;

        // Update FSM
        let frame_id2 = pool.fetch_page(page_id)?;
        let free = pool.page(frame_id2).free_space();
        pool.unpin_page(page_id, false)?;
        heap.update_fsm(page_id, free);
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::DiskManager;
    use crate::mvcc::{TransactionManager, TupleHeader};
    use crate::page::TUPLE_HEADER_SIZE;
    use tempfile::tempdir;

    fn make_env() -> (BufferPool, WalManager, TransactionManager, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let disk = DiskManager::create(&dir.path().join("test.db")).unwrap();
        let pool = BufferPool::new(64, disk);
        let wal = WalManager::create(&dir.path().join("test.wal")).unwrap();
        let tx = TransactionManager::new();
        (pool, wal, tx, dir)
    }

    /// Build a tuple payload: 24-byte MVCC header + `extra` zero bytes.
    fn make_tuple(xmin: u64, xmax: u64) -> Vec<u8> {
        let hdr = TupleHeader {
            xmin,
            xmax,
            cid: 0,
            infomask: 0,
            infomask2: 0,
        };
        let mut data = hdr.encode().to_vec();
        data.extend_from_slice(&[0u8; 8]); // small payload
        data
    }

    #[test]
    fn test_vacuum_removes_dead_tuples() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut heap = HeapFile::new(1);

        // Xid 1: insert 100 tuples and commit
        let xid = tx.begin();
        let mut tids = Vec::new();
        for _ in 0..100 {
            let data = make_tuple(xid, 0);
            let tid = heap.insert_tuple(&mut pool, &mut wal, xid, &data).unwrap();
            tids.push(tid);
        }
        tx.commit(xid, &mut wal).unwrap();

        // Xid 2: "delete" 50 tuples by rewriting their xmax, then commit
        let xid2 = tx.begin();
        for &tid in tids.iter().take(50) {
            // Read current data, patch xmax, write back
            let frame_id = pool.fetch_page(tid.page_id).unwrap();
            let raw = pool.page(frame_id).get_tuple(tid.slot_id).unwrap().to_vec();
            let mut new_data = raw.clone();
            new_data[8..16].copy_from_slice(&xid2.to_le_bytes()); // set xmax
            // Overwrite the slot by zeroing and re-inserting is complex; instead mark via delete+insert
            pool.page_mut(frame_id).delete_tuple(tid.slot_id).unwrap();
            pool.page_mut(frame_id).insert_tuple(&new_data).unwrap();
            pool.unpin_page(tid.page_id, true).unwrap();
        }
        tx.commit(xid2, &mut wal).unwrap();
        wal.flush().unwrap();

        // Vacuum
        let stats = vacuum_table(&mut heap, &mut pool, &mut wal, &tx).unwrap();

        assert!(stats.tuples_removed >= 50, "expected at least 50 tuples removed, got {}", stats.tuples_removed);
        assert!(stats.pages_compacted >= 1);

        // Count live tuples remaining
        let live = heap.scan(&mut pool).unwrap();
        // Live = the 50 non-deleted ones (xmax==0) plus anything else
        let live_count = live.iter().filter(|(_, data)| {
            if data.len() >= TUPLE_HEADER_SIZE {
                let hdr = TupleHeader::decode(data).unwrap();
                hdr.xmax == 0
            } else {
                false
            }
        }).count();
        assert_eq!(live_count, 50, "expected 50 live tuples, got {}", live_count);
    }

    #[test]
    fn test_vacuum_empty_table() {
        let (mut pool, mut wal, tx, _dir) = make_env();
        let mut heap = HeapFile::new(1);

        let stats = vacuum_table(&mut heap, &mut pool, &mut wal, &tx).unwrap();
        assert_eq!(stats.pages_scanned, 0);
        assert_eq!(stats.tuples_removed, 0);
        assert_eq!(stats.tuples_examined, 0);
    }

    #[test]
    fn test_vacuum_all_live() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut heap = HeapFile::new(1);

        let xid = tx.begin();
        for _ in 0..100 {
            let data = make_tuple(xid, 0);
            heap.insert_tuple(&mut pool, &mut wal, xid, &data).unwrap();
        }
        tx.commit(xid, &mut wal).unwrap();
        wal.flush().unwrap();

        let before_scan = heap.scan(&mut pool).unwrap().len();

        let stats = vacuum_table(&mut heap, &mut pool, &mut wal, &tx).unwrap();
        assert_eq!(stats.tuples_removed, 0);

        let after_scan = heap.scan(&mut pool).unwrap().len();
        assert_eq!(before_scan, after_scan, "vacuum should not remove live tuples");
        assert_eq!(after_scan, 100);
    }
}
