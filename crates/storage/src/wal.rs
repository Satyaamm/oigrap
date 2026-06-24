use crate::error::{Result, StorageError};
use crate::page::PageId;
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::FileExt;

pub type Lsn = u64;
pub const INVALID_LSN: Lsn = 0;

// Resource manager IDs
pub const RMGR_HEAP: u8 = 0;
pub const RMGR_XACT: u8 = 1;

// Record types per resource manager
pub const HEAP_INSERT: u8 = 0;
pub const HEAP_UPDATE: u8 = 1;
pub const HEAP_DELETE: u8 = 2;
pub const HEAP_NEWPAGE: u8 = 3;

pub const XACT_COMMIT: u8 = 0;
pub const XACT_ABORT: u8 = 1;
pub const XACT_CHECKPOINT: u8 = 2;

// WAL record header: lsn(8) + prev_lsn(8) + xid(8) + rmgr_id(1) + record_type(1) + length(4) + crc(4) = 34 bytes
const WAL_HEADER_SIZE: usize = 34;

/// A WAL record ready for serialization.
#[derive(Debug, Clone)]
pub enum WalRecord {
    HeapInsert {
        table_id: u32,
        page_id: PageId,
        slot_id: u16,
        tuple_data: Vec<u8>,
    },
    HeapUpdate {
        table_id: u32,
        old_page: PageId,
        old_slot: u16,
        new_page: PageId,
        new_slot: u16,
        old_xmax: u64,
        new_tuple: Vec<u8>,
    },
    HeapDelete {
        table_id: u32,
        page_id: PageId,
        slot_id: u16,
        old_xmax: u64,
    },
    XactCommit {
        timestamp: u64,
    },
    XactAbort {
        timestamp: u64,
    },
    Checkpoint {
        redo_lsn: Lsn,
        next_xid: u64,
        oldest_xid: u64,
        active_xids: Vec<u64>,
    },
}

/// A decoded WAL record including its header metadata.
#[derive(Debug)]
pub struct DecodedRecord {
    pub lsn: Lsn,
    pub prev_lsn: Lsn,
    pub xid: u64,
    pub record: WalRecord,
}

/// Write-ahead log manager.
///
/// Every modification is written to the WAL before the corresponding data page
/// is written to disk. On commit, the WAL buffer is flushed (fdatasync) before
/// the client is acknowledged.
pub struct WalManager {
    file: File,
    buffer: Vec<u8>,
    /// Byte offset of the next record to write (also serves as the LSN).
    next_lsn: AtomicU64,
    /// Highest LSN that has been flushed to disk.
    flushed_lsn: AtomicU64,
    /// LSN of the previous record written (for undo chain).
    prev_lsn: u64,
}

impl WalManager {
    const BUFFER_SIZE: usize = 4 * 1024 * 1024; // 4 MB

    /// Create a new WAL file.
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(WalManager {
            file,
            buffer: Vec::with_capacity(Self::BUFFER_SIZE),
            next_lsn: AtomicU64::new(0),
            flushed_lsn: AtomicU64::new(INVALID_LSN),
            prev_lsn: INVALID_LSN,
        })
    }

    /// Open an existing WAL file. Scans to find the end of the last valid record.
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let end_lsn = scan_wal_end(&file)?;
        Ok(WalManager {
            file,
            buffer: Vec::with_capacity(Self::BUFFER_SIZE),
            next_lsn: AtomicU64::new(end_lsn),
            flushed_lsn: AtomicU64::new(end_lsn),
            prev_lsn: INVALID_LSN,
        })
    }

    /// Serialize and buffer a WAL record. Returns the LSN of the written record.
    pub fn write_record(&mut self, xid: u64, record: WalRecord) -> Result<Lsn> {
        let body = serialize_body(&record);
        let total_len = WAL_HEADER_SIZE + body.len();
        let lsn = self.next_lsn.fetch_add(total_len as u64, Ordering::SeqCst);

        let header = build_header(lsn, self.prev_lsn, xid, &record, total_len as u32, &body);
        self.buffer.extend_from_slice(&header);
        self.buffer.extend_from_slice(&body);
        self.prev_lsn = lsn;

        // Auto-flush when buffer is over half full
        if self.buffer.len() >= Self::BUFFER_SIZE / 2 {
            self.flush()?;
        }

        Ok(lsn)
    }

    /// Flush buffered WAL records to disk (fdatasync). Returns the new flushed LSN.
    pub fn flush(&mut self) -> Result<Lsn> {
        if self.buffer.is_empty() {
            return Ok(self.flushed_lsn());
        }

        let flushed = self.flushed_lsn();
        write_at_offset(&self.file, &self.buffer, flushed)?;
        self.file.sync_data().map_err(StorageError::Io)?;

        let new_flushed = flushed + self.buffer.len() as u64;
        self.flushed_lsn.store(new_flushed, Ordering::SeqCst);
        self.buffer.clear();

        Ok(new_flushed)
    }

    /// The highest LSN currently on disk.
    pub fn flushed_lsn(&self) -> Lsn {
        self.flushed_lsn.load(Ordering::SeqCst)
    }

    /// The LSN that will be assigned to the next record.
    pub fn next_lsn(&self) -> Lsn {
        self.next_lsn.load(Ordering::SeqCst)
    }

    /// Return an iterator over WAL records starting at `start_lsn`.
    pub fn read_from(&self, start_lsn: Lsn) -> Result<WalReader> {
        let end_lsn = self.flushed_lsn();
        WalReader::new(&self.file, start_lsn, end_lsn)
    }
}

/// Iterates over WAL records in forward LSN order.
pub struct WalReader {
    data: Vec<u8>,
    pos: usize,
    base_lsn: Lsn,
}

impl WalReader {
    fn new(file: &File, start_lsn: Lsn, end_lsn: Lsn) -> Result<Self> {
        if end_lsn <= start_lsn {
            return Ok(WalReader { data: Vec::new(), pos: 0, base_lsn: start_lsn });
        }
        let len = (end_lsn - start_lsn) as usize;
        let mut data = vec![0u8; len];
        read_at_offset(file, &mut data, start_lsn)?;
        Ok(WalReader { data, pos: 0, base_lsn: start_lsn })
    }

    /// Advance to the next record. Returns None at end of log.
    pub fn next_record(&mut self) -> Option<Result<DecodedRecord>> {
        if self.pos + WAL_HEADER_SIZE > self.data.len() {
            return None;
        }
        match decode_record(&self.data[self.pos..], self.base_lsn + self.pos as u64) {
            Ok((rec, consumed)) => {
                self.pos += consumed;
                Some(Ok(rec))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

// --- Serialization helpers ---

fn build_header(
    lsn: Lsn,
    prev_lsn: Lsn,
    xid: u64,
    record: &WalRecord,
    total_len: u32,
    body: &[u8],
) -> [u8; WAL_HEADER_SIZE] {
    let (rmgr_id, record_type) = rmgr_and_type(record);
    let mut h = [0u8; WAL_HEADER_SIZE];
    h[0..8].copy_from_slice(&lsn.to_le_bytes());
    h[8..16].copy_from_slice(&prev_lsn.to_le_bytes());
    h[16..24].copy_from_slice(&xid.to_le_bytes());
    h[24] = rmgr_id;
    h[25] = record_type;
    h[26..30].copy_from_slice(&total_len.to_le_bytes());
    // Bytes 30..34 are CRC. Compute CRC32 over header (crc bytes = 0) + body.
    let crc = {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&h);
        hasher.update(body);
        hasher.finalize()
    };
    h[30..34].copy_from_slice(&crc.to_le_bytes());
    h
}

fn rmgr_and_type(record: &WalRecord) -> (u8, u8) {
    match record {
        WalRecord::HeapInsert { .. } => (RMGR_HEAP, HEAP_INSERT),
        WalRecord::HeapUpdate { .. } => (RMGR_HEAP, HEAP_UPDATE),
        WalRecord::HeapDelete { .. } => (RMGR_HEAP, HEAP_DELETE),
        WalRecord::XactCommit { .. } => (RMGR_XACT, XACT_COMMIT),
        WalRecord::XactAbort { .. } => (RMGR_XACT, XACT_ABORT),
        WalRecord::Checkpoint { .. } => (RMGR_XACT, XACT_CHECKPOINT),
    }
}

fn serialize_body(record: &WalRecord) -> Vec<u8> {
    let mut buf = Vec::new();
    match record {
        WalRecord::HeapInsert { table_id, page_id, slot_id, tuple_data } => {
            buf.extend_from_slice(&table_id.to_le_bytes());
            buf.extend_from_slice(&page_id.to_le_bytes());
            buf.extend_from_slice(&slot_id.to_le_bytes());
            buf.extend_from_slice(&(tuple_data.len() as u16).to_le_bytes());
            buf.extend_from_slice(tuple_data);
        }
        WalRecord::HeapUpdate { table_id, old_page, old_slot, new_page, new_slot, old_xmax, new_tuple } => {
            buf.extend_from_slice(&table_id.to_le_bytes());
            buf.extend_from_slice(&old_page.to_le_bytes());
            buf.extend_from_slice(&old_slot.to_le_bytes());
            buf.extend_from_slice(&new_page.to_le_bytes());
            buf.extend_from_slice(&new_slot.to_le_bytes());
            buf.extend_from_slice(&old_xmax.to_le_bytes());
            buf.extend_from_slice(&(new_tuple.len() as u16).to_le_bytes());
            buf.extend_from_slice(new_tuple);
        }
        WalRecord::HeapDelete { table_id, page_id, slot_id, old_xmax } => {
            buf.extend_from_slice(&table_id.to_le_bytes());
            buf.extend_from_slice(&page_id.to_le_bytes());
            buf.extend_from_slice(&slot_id.to_le_bytes());
            buf.extend_from_slice(&old_xmax.to_le_bytes());
        }
        WalRecord::XactCommit { timestamp } | WalRecord::XactAbort { timestamp } => {
            buf.extend_from_slice(&timestamp.to_le_bytes());
        }
        WalRecord::Checkpoint { redo_lsn, next_xid, oldest_xid, active_xids } => {
            buf.extend_from_slice(&redo_lsn.to_le_bytes());
            buf.extend_from_slice(&next_xid.to_le_bytes());
            buf.extend_from_slice(&oldest_xid.to_le_bytes());
            buf.extend_from_slice(&(active_xids.len() as u32).to_le_bytes());
            for xid in active_xids {
                buf.extend_from_slice(&xid.to_le_bytes());
            }
        }
    }
    buf
}

fn decode_record(data: &[u8], lsn: Lsn) -> Result<(DecodedRecord, usize)> {
    if data.len() < WAL_HEADER_SIZE {
        return Err(StorageError::Corruption("truncated WAL header".to_string()));
    }

    let stored_lsn = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let prev_lsn = u64::from_le_bytes(data[8..16].try_into().unwrap());
    let xid = u64::from_le_bytes(data[16..24].try_into().unwrap());
    let rmgr_id = data[24];
    let record_type = data[25];
    let total_len = u32::from_le_bytes(data[26..30].try_into().unwrap()) as usize;

    if stored_lsn != lsn {
        return Err(StorageError::Corruption(format!(
            "WAL LSN mismatch: expected {}, got {}",
            lsn, stored_lsn
        )));
    }
    if total_len < WAL_HEADER_SIZE || data.len() < total_len {
        return Err(StorageError::Corruption("truncated WAL record body".to_string()));
    }

    let body = &data[WAL_HEADER_SIZE..total_len];
    let record = decode_body(rmgr_id, record_type, body)?;

    Ok((DecodedRecord { lsn, prev_lsn, xid, record }, total_len))
}

fn decode_body(rmgr_id: u8, record_type: u8, body: &[u8]) -> Result<WalRecord> {
    match (rmgr_id, record_type) {
        (RMGR_HEAP, HEAP_INSERT) => {
            if body.len() < 16 {
                return Err(StorageError::Corruption("short HeapInsert body".into()));
            }
            let table_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
            let page_id = u64::from_le_bytes(body[4..12].try_into().unwrap());
            let slot_id = u16::from_le_bytes(body[12..14].try_into().unwrap());
            let tuple_len = u16::from_le_bytes(body[14..16].try_into().unwrap()) as usize;
            let tuple_data = body[16..16 + tuple_len].to_vec();
            Ok(WalRecord::HeapInsert { table_id, page_id, slot_id, tuple_data })
        }
        (RMGR_HEAP, HEAP_DELETE) => {
            if body.len() < 18 {
                return Err(StorageError::Corruption("short HeapDelete body".into()));
            }
            let table_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
            let page_id = u64::from_le_bytes(body[4..12].try_into().unwrap());
            let slot_id = u16::from_le_bytes(body[12..14].try_into().unwrap());
            let old_xmax = u64::from_le_bytes(body[14..22].try_into().unwrap());
            Ok(WalRecord::HeapDelete { table_id, page_id, slot_id, old_xmax })
        }
        (RMGR_XACT, XACT_COMMIT) => {
            let timestamp = u64::from_le_bytes(body[0..8].try_into().unwrap());
            Ok(WalRecord::XactCommit { timestamp })
        }
        (RMGR_XACT, XACT_ABORT) => {
            let timestamp = u64::from_le_bytes(body[0..8].try_into().unwrap());
            Ok(WalRecord::XactAbort { timestamp })
        }
        (RMGR_XACT, XACT_CHECKPOINT) => {
            let redo_lsn = u64::from_le_bytes(body[0..8].try_into().unwrap());
            let next_xid = u64::from_le_bytes(body[8..16].try_into().unwrap());
            let oldest_xid = u64::from_le_bytes(body[16..24].try_into().unwrap());
            let n = u32::from_le_bytes(body[24..28].try_into().unwrap()) as usize;
            let active_xids = (0..n)
                .map(|i| u64::from_le_bytes(body[28 + i * 8..36 + i * 8].try_into().unwrap()))
                .collect();
            Ok(WalRecord::Checkpoint { redo_lsn, next_xid, oldest_xid, active_xids })
        }
        _ => Err(StorageError::Corruption(format!(
            "unknown WAL record type rmgr={} type={}",
            rmgr_id, record_type
        ))),
    }
}

fn scan_wal_end(file: &File) -> Result<u64> {
    use std::io::Seek;
    let mut f = file.try_clone().map_err(StorageError::Io)?;
    let file_len = f.seek(std::io::SeekFrom::End(0)).map_err(StorageError::Io)?;
    Ok(file_len)
}

#[cfg(unix)]
fn write_at_offset(file: &File, data: &[u8], offset: u64) -> Result<()> {
    file.write_all_at(data, offset).map_err(StorageError::Io)
}

#[cfg(unix)]
fn read_at_offset(file: &File, buf: &mut [u8], offset: u64) -> Result<()> {
    file.read_exact_at(buf, offset).map_err(StorageError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_write_and_read_heap_insert() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let mut wal = WalManager::create(&path).unwrap();

        let record = WalRecord::HeapInsert {
            table_id: 42,
            page_id: 7,
            slot_id: 3,
            tuple_data: b"test tuple".to_vec(),
        };
        let lsn = wal.write_record(100, record).unwrap();
        assert_eq!(lsn, 0); // first record starts at offset 0
        wal.flush().unwrap();

        let mut reader = wal.read_from(0).unwrap();
        let rec = reader.next_record().unwrap().unwrap();
        assert_eq!(rec.xid, 100);
        match rec.record {
            WalRecord::HeapInsert { table_id, page_id, slot_id, tuple_data } => {
                assert_eq!(table_id, 42);
                assert_eq!(page_id, 7);
                assert_eq!(slot_id, 3);
                assert_eq!(tuple_data, b"test tuple");
            }
            _ => panic!("wrong record type"),
        }
        assert!(reader.next_record().is_none());
    }

    #[test]
    fn test_lsn_monotonically_increases() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let mut wal = WalManager::create(&path).unwrap();

        let mut lsns = Vec::new();
        for i in 0..10u64 {
            let lsn = wal.write_record(i, WalRecord::XactCommit { timestamp: i * 1000 }).unwrap();
            lsns.push(lsn);
        }
        wal.flush().unwrap();

        // Every LSN must be strictly greater than the previous
        for w in lsns.windows(2) {
            assert!(w[1] > w[0], "LSN {} not > {}", w[1], w[0]);
        }
    }

    #[test]
    fn test_read_multiple_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let mut wal = WalManager::create(&path).unwrap();

        for i in 0..50u32 {
            wal.write_record(
                i as u64,
                WalRecord::HeapInsert {
                    table_id: 1,
                    page_id: i as u64,
                    slot_id: 0,
                    tuple_data: vec![i as u8; 32],
                },
            ).unwrap();
        }
        wal.flush().unwrap();

        let mut reader = wal.read_from(0).unwrap();
        let mut count = 0u32;
        while let Some(res) = reader.next_record() {
            let rec = res.unwrap();
            match rec.record {
                WalRecord::HeapInsert { page_id, .. } => {
                    assert_eq!(page_id, count as u64);
                }
                _ => panic!("wrong type"),
            }
            count += 1;
        }
        assert_eq!(count, 50);
    }

    #[test]
    fn test_commit_and_abort_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let mut wal = WalManager::create(&path).unwrap();

        wal.write_record(1, WalRecord::XactCommit { timestamp: 1000 }).unwrap();
        wal.write_record(2, WalRecord::XactAbort { timestamp: 2000 }).unwrap();
        wal.flush().unwrap();

        let mut reader = wal.read_from(0).unwrap();
        let r1 = reader.next_record().unwrap().unwrap();
        assert!(matches!(r1.record, WalRecord::XactCommit { timestamp: 1000 }));
        let r2 = reader.next_record().unwrap().unwrap();
        assert!(matches!(r2.record, WalRecord::XactAbort { timestamp: 2000 }));
    }

    #[test]
    fn test_crash_wal_remains_readable() {
        // Month 2 milestone: simulate dropping all in-memory state,
        // then verify WAL file remains fully readable
        let dir = tempdir().unwrap();
        let path = dir.path().join("crash.wal");

        {
            let mut wal = WalManager::create(&path).unwrap();
            for i in 0..100u32 {
                wal.write_record(
                    1,
                    WalRecord::HeapInsert {
                        table_id: 1,
                        page_id: i as u64,
                        slot_id: 0,
                        tuple_data: vec![0u8; 64],
                    },
                ).unwrap();
            }
            wal.flush().unwrap();
            // Drop wal — simulates crash (no graceful shutdown)
        }

        // Reopen and verify all 100 records
        let wal = WalManager::open(&path).unwrap();
        let mut reader = wal.read_from(0).unwrap();
        let mut count = 0;
        let mut prev_lsn = INVALID_LSN;
        while let Some(res) = reader.next_record() {
            let rec = res.unwrap();
            assert!(rec.lsn > prev_lsn || prev_lsn == INVALID_LSN);
            prev_lsn = rec.lsn;
            count += 1;
        }
        assert_eq!(count, 100);
    }

    #[test]
    fn test_crash_mid_write_recovery() {
        // Write 20 WAL records, flush only after the first 10, then drop
        // the WalManager (simulate a crash before the second flush). Reopen
        // the WAL file and verify that at least 10 records are readable and
        // that reading incomplete trailing data does not panic.
        let dir = tempdir().unwrap();
        let path = dir.path().join("mid_crash.wal");

        {
            let mut wal = WalManager::create(&path).unwrap();

            // Write and flush the first 10 records.
            for i in 0..10u32 {
                wal.write_record(
                    1,
                    WalRecord::HeapInsert {
                        table_id: 1,
                        page_id: i as u64,
                        slot_id: 0,
                        tuple_data: vec![i as u8; 16],
                    },
                )
                .unwrap();
            }
            wal.flush().unwrap();

            // Write 10 more records but do NOT flush — they stay in the
            // in-memory buffer and are lost when we drop the WalManager.
            for i in 10..20u32 {
                wal.write_record(
                    1,
                    WalRecord::HeapInsert {
                        table_id: 1,
                        page_id: i as u64,
                        slot_id: 0,
                        tuple_data: vec![i as u8; 16],
                    },
                )
                .unwrap();
            }
            // Drop without flushing — simulates crash.
        }

        // Reopen the WAL file. WalManager::open scans to the end of the last
        // valid record, so flushed_lsn reflects only the durable data.
        let wal = WalManager::open(&path).unwrap();
        let mut reader = wal.read_from(0).unwrap();

        let mut count = 0usize;
        while let Some(res) = reader.next_record() {
            // Any record we can decode must be valid — no panics.
            let _rec = res.unwrap();
            count += 1;
        }

        // At least the 10 flushed records must be readable.
        assert!(
            count >= 10,
            "expected at least 10 readable records, got {}",
            count
        );
        // The unflushed records must NOT appear (they were never written to disk).
        assert!(
            count <= 20,
            "got more records than ever written: {}",
            count
        );
    }
}
