//! Spill-to-disk serialization for hash join partitioning.
//! Rows are written as: [u32 col_count][per col: u8 tag + value bytes]
//! Tags: 0=Null, 1=Bool(1 byte), 2=Int64(8 LE), 3=Float64(8 LE), 4=Text(u32 LE len + bytes)

use crate::Value;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};

pub struct SpillWriter {
    writer: BufWriter<File>,
    pub path: std::path::PathBuf,
    pub row_count: usize,
}

impl SpillWriter {
    pub fn new() -> std::io::Result<Self> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("oigrap_spill_{}.bin", id));
        let file = File::create(&path)?;
        Ok(SpillWriter { writer: BufWriter::new(file), path, row_count: 0 })
    }

    pub fn write_row(&mut self, row: &[Value]) -> std::io::Result<()> {
        let n = row.len() as u32;
        self.writer.write_all(&n.to_le_bytes())?;
        for v in row {
            match v {
                Value::Null => self.writer.write_all(&[0])?,
                Value::Bool(b) => {
                    self.writer.write_all(&[1])?;
                    self.writer.write_all(&[*b as u8])?;
                }
                Value::Int64(n) => {
                    self.writer.write_all(&[2])?;
                    self.writer.write_all(&n.to_le_bytes())?;
                }
                Value::Float64(f) => {
                    self.writer.write_all(&[3])?;
                    self.writer.write_all(&f.to_le_bytes())?;
                }
                Value::Text(s) => {
                    self.writer.write_all(&[4])?;
                    let bytes = s.as_bytes();
                    self.writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
                    self.writer.write_all(bytes)?;
                }
            }
        }
        self.row_count += 1;
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<SpillFile> {
        self.writer.flush()?;
        Ok(SpillFile { path: self.path, row_count: self.row_count })
    }
}

pub struct SpillFile {
    pub path: std::path::PathBuf,
    #[allow(dead_code)]
    pub row_count: usize,
}

impl SpillFile {
    pub fn reader(&self) -> std::io::Result<SpillReader> {
        let file = File::open(&self.path)?;
        Ok(SpillReader { reader: BufReader::new(file) })
    }
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct SpillReader {
    reader: BufReader<File>,
}

impl SpillReader {
    pub fn read_row(&mut self) -> std::io::Result<Option<Vec<Value>>> {
        let mut buf4 = [0u8; 4];
        match self.reader.read_exact(&mut buf4) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let col_count = u32::from_le_bytes(buf4) as usize;
        let mut row = Vec::with_capacity(col_count);
        for _ in 0..col_count {
            let mut tag = [0u8; 1];
            self.reader.read_exact(&mut tag)?;
            let v = match tag[0] {
                0 => Value::Null,
                1 => {
                    let mut b = [0u8; 1];
                    self.reader.read_exact(&mut b)?;
                    Value::Bool(b[0] != 0)
                }
                2 => {
                    let mut b = [0u8; 8];
                    self.reader.read_exact(&mut b)?;
                    Value::Int64(i64::from_le_bytes(b))
                }
                3 => {
                    let mut b = [0u8; 8];
                    self.reader.read_exact(&mut b)?;
                    Value::Float64(f64::from_le_bytes(b))
                }
                4 => {
                    let mut b = [0u8; 4];
                    self.reader.read_exact(&mut b)?;
                    let len = u32::from_le_bytes(b) as usize;
                    let mut s = vec![0u8; len];
                    self.reader.read_exact(&mut s)?;
                    Value::Text(String::from_utf8_lossy(&s).into_owned())
                }
                _ => Value::Null,
            };
            row.push(v);
        }
        Ok(Some(row))
    }

    pub fn into_rows(mut self) -> std::io::Result<Vec<Vec<Value>>> {
        let mut rows = Vec::new();
        while let Some(row) = self.read_row()? {
            rows.push(row);
        }
        Ok(rows)
    }
}

/// Partition rows into k buckets by hash(key_col) % k.
/// Returns a Vec of SpillFiles, one per bucket.
pub fn partition_rows(
    rows: &[Vec<Value>],
    key_col: usize,
    k: usize,
) -> std::io::Result<Vec<SpillFile>> {
    let mut writers: Vec<SpillWriter> = (0..k)
        .map(|_| SpillWriter::new())
        .collect::<std::io::Result<_>>()?;
    for row in rows {
        let key = format!("{:?}", &row[key_col]);
        let bucket = (hash_str(&key) % k as u64) as usize;
        writers[bucket].write_row(row)?;
    }
    writers.into_iter().map(|w| w.finish()).collect()
}

fn hash_str(s: &str) -> u64 {
    // FNV-1a 64-bit
    let mut h: u64 = 14695981039346656037;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spill_roundtrip() {
        let mut w = SpillWriter::new().unwrap();
        let row1 = vec![Value::Int64(42), Value::Text("hello".into()), Value::Null];
        let row2 = vec![Value::Bool(true), Value::Float64(3.14), Value::Int64(-1)];
        w.write_row(&row1).unwrap();
        w.write_row(&row2).unwrap();
        let file = w.finish().unwrap();
        assert_eq!(file.row_count, 2);

        let mut reader = file.reader().unwrap();
        assert_eq!(reader.read_row().unwrap(), Some(row1));
        assert_eq!(reader.read_row().unwrap(), Some(row2));
        assert_eq!(reader.read_row().unwrap(), None);
    }

    #[test]
    fn test_spill_file_cleaned_up_on_drop() {
        let path = {
            let mut w = SpillWriter::new().unwrap();
            w.write_row(&[Value::Int64(1)]).unwrap();
            let f = w.finish().unwrap();
            let p = f.path.clone();
            assert!(p.exists());
            p
            // f dropped here -> file deleted
        };
        assert!(!path.exists(), "spill file should be deleted on drop");
    }

    #[test]
    fn test_partition_rows() {
        let rows: Vec<Vec<Value>> = (0..100)
            .map(|i| vec![Value::Int64(i), Value::Text(format!("v{}", i))])
            .collect();
        let files = partition_rows(&rows, 0, 4).unwrap();
        assert_eq!(files.len(), 4);
        // All rows should be recoverable across all buckets
        let total: usize = files.iter().map(|f| f.row_count).sum();
        assert_eq!(total, 100);
    }

    #[test]
    fn test_empty_spill() {
        let w = SpillWriter::new().unwrap();
        let f = w.finish().unwrap();
        assert_eq!(f.row_count, 0);
        let rows = f.reader().unwrap().into_rows().unwrap();
        assert!(rows.is_empty());
    }
}
