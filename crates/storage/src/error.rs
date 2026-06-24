use crate::page::PageId;
use std::fmt;

#[derive(Debug)]
pub enum StorageError {
    Io(std::io::Error),
    BufferFull,
    PageNotFound(PageId),
    PageNotPinned(PageId),
    InsufficientSpace { needed: usize, available: usize },
    InvalidChecksum { page_id: PageId },
    SlotOutOfRange(u16),
    Corruption(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Io(e) => write!(f, "I/O error: {}", e),
            StorageError::BufferFull => write!(f, "buffer pool is full, all frames pinned"),
            StorageError::PageNotFound(id) => write!(f, "page {} not in buffer pool", id),
            StorageError::PageNotPinned(id) => write!(f, "page {} is not pinned", id),
            StorageError::InsufficientSpace { needed, available } => {
                write!(f, "insufficient page space: need {} bytes, {} available", needed, available)
            }
            StorageError::InvalidChecksum { page_id } => {
                write!(f, "checksum mismatch on page {}", page_id)
            }
            StorageError::SlotOutOfRange(slot) => write!(f, "slot {} out of range", slot),
            StorageError::Corruption(msg) => write!(f, "storage corruption: {}", msg),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::Io(e)
    }
}
