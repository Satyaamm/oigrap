#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use crate::buffer_pool::BufferPool;
    use crate::disk::DiskManager;
    use crate::error::StorageError;
    use crate::mvcc::TransactionManager;
    use crate::page::Page;
    use crate::wal::{WalManager, WalRecord};

    // ---------------------------------------------------------------------------
    // a. prop_page_insert_read_roundtrip
    // ---------------------------------------------------------------------------

    proptest! {
        #[test]
        fn prop_page_insert_read_roundtrip(
            tuples in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 1..200usize),
                1..30usize,
            )
        ) {
            let mut page = Page::new(1);
            let mut inserted: Vec<(u16, Vec<u8>)> = Vec::new();

            for tuple in &tuples {
                match page.insert_tuple(tuple) {
                    Ok(slot_id) => {
                        inserted.push((slot_id, tuple.clone()));
                    }
                    Err(StorageError::InsufficientSpace { .. }) => {
                        // Page full — stop inserting
                        break;
                    }
                    Err(e) => {
                        return Err(proptest::test_runner::TestCaseError::fail(
                            format!("unexpected insert error: {}", e),
                        ));
                    }
                }
            }

            for (slot_id, expected) in &inserted {
                let actual = page.get_tuple(*slot_id).map_err(|e| {
                    proptest::test_runner::TestCaseError::fail(format!(
                        "get_tuple({}) failed: {}",
                        slot_id, e
                    ))
                })?;
                prop_assert_eq!(
                    actual,
                    expected.as_slice(),
                    "mismatch at slot {}",
                    slot_id
                );
            }
        }
    }

    // ---------------------------------------------------------------------------
    // b. prop_wal_lsn_monotonic
    // ---------------------------------------------------------------------------

    proptest! {
        #[test]
        fn prop_wal_lsn_monotonic(
            xids in proptest::collection::vec(any::<u64>(), 1..50usize)
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("fuzz.wal");
            let mut wal = WalManager::create(&path).unwrap();

            let mut lsns: Vec<u64> = Vec::new();
            for &xid in &xids {
                let lsn = wal.write_record(
                    xid,
                    WalRecord::HeapInsert {
                        table_id: 1,
                        page_id: 0,
                        slot_id: 0,
                        tuple_data: vec![0u8; 8],
                    },
                ).unwrap();
                lsns.push(lsn);
            }

            wal.flush().unwrap();

            for w in lsns.windows(2) {
                prop_assert!(
                    w[1] > w[0],
                    "LSN {} is not strictly greater than {}",
                    w[1],
                    w[0]
                );
            }
        }
    }

    // ---------------------------------------------------------------------------
    // c. prop_mvcc_snapshot_isolation
    // ---------------------------------------------------------------------------

    proptest! {
        #[test]
        fn prop_mvcc_snapshot_isolation(
            commit_flags in prop::collection::vec(any::<bool>(), 2..20usize)
        ) {
            let dir = tempfile::tempdir().unwrap();
            let wal_path = dir.path().join("fuzz_mvcc.wal");
            let mut wal = WalManager::create(&wal_path).unwrap();
            let mut tx = TransactionManager::new();

            let mut committed_xids: Vec<u64> = Vec::new();
            let mut aborted_xids: Vec<u64> = Vec::new();

            for &commit in &commit_flags {
                let xid = tx.begin();
                if commit {
                    tx.commit(xid, &mut wal).unwrap();
                    committed_xids.push(xid);
                } else {
                    tx.abort(xid, &mut wal).unwrap();
                    aborted_xids.push(xid);
                }
            }

            let snap = tx.snapshot();

            for xid in &committed_xids {
                prop_assert!(
                    !snap.active.contains(xid),
                    "committed xid {} should not be in snapshot.active",
                    xid
                );
                prop_assert!(
                    *xid < snap.xmax,
                    "committed xid {} should be < snap.xmax {}",
                    xid,
                    snap.xmax
                );
            }

            for xid in &aborted_xids {
                prop_assert!(
                    !tx.is_committed(*xid),
                    "aborted xid {} should not be committed",
                    xid
                );
            }
        }
    }

    // ---------------------------------------------------------------------------
    // d. prop_buffer_pool_pin_unpin
    // ---------------------------------------------------------------------------

    proptest! {
        #[test]
        fn prop_buffer_pool_pin_unpin(
            page_id_seq in proptest::collection::vec(0u64..20u64, 1..40usize)
        ) {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("fuzz_pool.db");
            let disk = DiskManager::create(&db_path).unwrap();
            let mut pool = BufferPool::new(8, disk);

            // Track which page IDs have been allocated (logical 0..20 -> actual PageId).
            // We allocate on first encounter.
            let mut allocated: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();

            for &logical_id in &page_id_seq {
                // Ensure the page is allocated.
                let actual_page_id = *allocated.entry(logical_id).or_insert_with(|| {
                    let (pid, _frame_id) = pool.new_page().unwrap();
                    pool.unpin_page(pid, false).unwrap();
                    pid
                });

                pool.fetch_page(actual_page_id).unwrap();
                pool.unpin_page(actual_page_id, false).unwrap();
            }

            pool.flush_all().unwrap();
        }
    }
}
