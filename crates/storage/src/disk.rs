use crate::error::{Result, StorageError};
use crate::page::{PageId, PAGE_SIZE};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::FileExt;

const MAGIC: &[u8; 8] = b"OIGRAP\0\0";
const FORMAT_VERSION: u32 = 1;

/// Manages raw page I/O against the database file.
///
/// All reads and writes use positional I/O (pread/pwrite) so the DiskManager
/// is safe to use from multiple threads simultaneously.
pub struct DiskManager {
    file: File,
    /// Next page ID to hand out. Page 0 is the header page, so user pages start at 1.
    next_page_id: AtomicU64,
}

impl DiskManager {
    /// Create a new database file. Fails if the file already exists.
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;

        let dm = DiskManager {
            file,
            next_page_id: AtomicU64::new(1), // page 0 is reserved for header
        };
        dm.write_header_page(1)?;
        Ok(dm)
    }

    /// Open an existing database file and restore the page count.
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        let mut header_buf = [0u8; PAGE_SIZE];
        read_at(&file, &mut header_buf, 0)?;

        if &header_buf[0..8] != MAGIC {
            return Err(StorageError::Corruption(
                "invalid magic bytes in header page".to_string(),
            ));
        }

        let next_page_id =
            u64::from_le_bytes(header_buf[8..16].try_into().unwrap());

        Ok(DiskManager {
            file,
            next_page_id: AtomicU64::new(next_page_id),
        })
    }

    /// Read an 8KB page from disk into `buf`.
    pub fn read_page(&self, page_id: PageId, buf: &mut [u8; PAGE_SIZE]) -> Result<()> {
        let offset = page_offset(page_id);
        read_at(&self.file, buf, offset)
    }

    /// Write an 8KB page to disk. Extends the file if necessary.
    pub fn write_page(&self, page_id: PageId, data: &[u8; PAGE_SIZE]) -> Result<()> {
        let offset = page_offset(page_id);
        write_at(&self.file, data, offset)
    }

    /// Allocate a new page ID and extend the file to cover it.
    ///
    /// The new page is zero-initialized on disk. Returns the new PageId.
    pub fn allocate_page(&self) -> Result<PageId> {
        let page_id = self.next_page_id.fetch_add(1, Ordering::SeqCst);
        // Write zeros to that position so the file is extended
        let blank = [0u8; PAGE_SIZE];
        self.write_page(page_id, &blank)?;
        // Persist updated page count to the header
        self.write_header_page(page_id + 1)?;
        Ok(page_id)
    }

    /// Number of pages currently allocated (includes header page 0).
    pub fn page_count(&self) -> u64 {
        self.next_page_id.load(Ordering::SeqCst)
    }

    /// Flush OS write buffers to disk (fdatasync).
    pub fn sync(&self) -> Result<()> {
        self.file.sync_data().map_err(StorageError::Io)
    }

    // --- Internal ---

    fn write_header_page(&self, next_page_id: u64) -> Result<()> {
        let mut header = [0u8; PAGE_SIZE];
        header[0..8].copy_from_slice(MAGIC);
        header[8..16].copy_from_slice(&next_page_id.to_le_bytes());
        header[16..20].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        write_at(&self.file, &header, 0)
    }
}

fn page_offset(page_id: PageId) -> u64 {
    page_id * PAGE_SIZE as u64
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> Result<()> {
    file.read_exact_at(buf, offset).map_err(StorageError::Io)
}

#[cfg(unix)]
fn write_at(file: &File, data: &[u8], offset: u64) -> Result<()> {
    file.write_all_at(data, offset).map_err(StorageError::Io)
}

#[cfg(not(unix))]
compile_error!("oigrap storage engine currently requires a Unix OS (macOS or Linux)");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::Page;
    use tempfile::tempdir;

    #[test]
    fn test_create_write_read_page() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        let dm = DiskManager::create(&path).unwrap();
        let page_id = dm.allocate_page().unwrap();
        assert_eq!(page_id, 1);

        let mut write_buf = [0u8; PAGE_SIZE];
        write_buf[0..5].copy_from_slice(b"hello");
        dm.write_page(page_id, &write_buf).unwrap();

        let mut read_buf = [0u8; PAGE_SIZE];
        dm.read_page(page_id, &mut read_buf).unwrap();
        assert_eq!(&read_buf[0..5], b"hello");
        assert_eq!(read_buf[5..], write_buf[5..]);
    }

    #[test]
    fn test_open_restores_page_count() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        {
            let dm = DiskManager::create(&path).unwrap();
            dm.allocate_page().unwrap(); // page 1
            dm.allocate_page().unwrap(); // page 2
            dm.allocate_page().unwrap(); // page 3
            assert_eq!(dm.page_count(), 4);
        }

        let dm2 = DiskManager::open(&path).unwrap();
        assert_eq!(dm2.page_count(), 4);
    }

    #[test]
    fn test_open_rejects_corrupt_magic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.db");

        // Write garbage
        std::fs::write(&path, vec![0u8; PAGE_SIZE]).unwrap();
        assert!(matches!(
            DiskManager::open(&path),
            Err(StorageError::Corruption(_))
        ));
    }

    #[test]
    fn test_allocate_sequential_page_ids() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let dm = DiskManager::create(&path).unwrap();

        let ids: Vec<PageId> = (0..20).map(|_| dm.allocate_page().unwrap()).collect();
        assert_eq!(ids, (1u64..=20).collect::<Vec<_>>());
    }

    #[test]
    fn test_data_persists_across_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");

        let page_id;
        let mut expected = [0u8; PAGE_SIZE];
        expected[100] = 0xAB;
        expected[200] = 0xCD;

        {
            let dm = DiskManager::create(&path).unwrap();
            page_id = dm.allocate_page().unwrap();
            dm.write_page(page_id, &expected).unwrap();
            dm.sync().unwrap();
        }

        {
            let dm = DiskManager::open(&path).unwrap();
            let mut actual = [0u8; PAGE_SIZE];
            dm.read_page(page_id, &mut actual).unwrap();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn test_page_count_starts_at_one() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let dm = DiskManager::create(&path).unwrap();
        // Only page 0 (header) exists initially; next_page_id = 1
        assert_eq!(dm.page_count(), 1);
    }

    #[test]
    fn test_write_full_page_content() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let dm = DiskManager::create(&path).unwrap();

        let page_id = dm.allocate_page().unwrap();

        // Write a Page struct's bytes
        let mut page = Page::new(page_id);
        page.insert_tuple(b"integration test tuple").unwrap();
        page.update_checksum();

        dm.write_page(page_id, page.as_bytes()).unwrap();

        let mut buf = [0u8; PAGE_SIZE];
        dm.read_page(page_id, &mut buf).unwrap();
        let page2 = Page::from_bytes(buf);

        assert_eq!(page2.page_id(), page_id);
        assert_eq!(page2.get_tuple(0).unwrap(), b"integration test tuple");
        assert!(page2.verify_checksum());
    }
}
