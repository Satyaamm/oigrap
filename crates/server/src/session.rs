use oigrap_sql::{Engine, QueryResult, SqlError, Value};
use oigrap_storage::{BufferPool, DiskManager, TransactionManager, WalManager, redo_recover, undo_recover};
use std::collections::HashMap;
use std::path::Path;
use tempfile::TempDir;

/// Tracks whether the database directory is temporary (in-memory test) or persistent (on-disk).
#[allow(dead_code)]
enum DbDir {
    Temp(TempDir),
    Persistent(std::path::PathBuf),
}

/// Globally shared database handle. One instance lives for the lifetime of the server.
pub struct DbHandle {
    pub engine: Engine,
    pub pool: BufferPool,
    pub wal: WalManager,
    pub tx: TransactionManager,
    #[allow(dead_code)]
    pub preloaded_columnar: HashMap<String, oigrap_storage::ColumnarStore>,
    _dir: DbDir,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct PreparedStmt {
    pub name: String,
    pub query: String,
    pub param_types: Vec<u32>,
}

#[allow(dead_code)]
pub struct Portal {
    pub stmt: PreparedStmt,
    pub params: Vec<Option<Vec<u8>>>,
    pub param_formats: Vec<i16>,
    pub result_formats: Vec<i16>,
    pub result: Option<QueryResult>,
    pub cursor: usize,
}

// Keep type alias so any stray references compile without touching connection.rs imports.
#[allow(dead_code)]
pub type Session = DbHandle;

impl DbHandle {
    pub fn new() -> std::io::Result<Self> {
        let dir = tempfile::tempdir()?;
        let disk = DiskManager::create(&dir.path().join("db"))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let pool = BufferPool::new(256, disk);
        let wal = WalManager::create(&dir.path().join("wal"))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let tx = TransactionManager::new();
        let engine = Engine::new();
        Ok(DbHandle {
            engine,
            pool,
            wal,
            tx,
            preloaded_columnar: HashMap::new(),
            _dir: DbDir::Temp(dir),
        })
    }

    #[allow(dead_code)]
    /// Open an existing persistent database directory.
    ///
    /// If the directory already contains a database (db + wal files), opens them and
    /// flushes the WAL to ensure consistency before serving queries.
    /// This is the ARIES recovery entry point: after opening the WAL we call
    /// `wal.flush()` which ensures all buffered records are written to disk.
    /// A full redo/undo pass would require wiring `wal.read_from(redo_lsn)` into
    /// `BufferPool` — that integration is left for a future milestone.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        let db_path = dir.join("db");
        let wal_path = dir.join("wal");

        let disk = if db_path.exists() {
            DiskManager::open(&db_path)
                .map_err(|e| std::io::Error::other(e.to_string()))?
        } else {
            DiskManager::create(&db_path)
                .map_err(|e| std::io::Error::other(e.to_string()))?
        };

        let mut pool = BufferPool::new(256, disk);

        let mut wal = if wal_path.exists() {
            WalManager::open(&wal_path)
                .map_err(|e| std::io::Error::other(e.to_string()))?
        } else {
            WalManager::create(&wal_path)
                .map_err(|e| std::io::Error::other(e.to_string()))?
        };

        // ARIES recovery: flush, redo, then undo.
        wal.flush().map_err(|e| std::io::Error::other(e.to_string()))?;

        let mut tx = TransactionManager::new();

        // Redo pass: replay WAL records onto pages whose on-disk LSN is behind.
        redo_recover(&wal, &mut pool)
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        // Undo pass: mark transactions active at crash as aborted so MVCC visibility
        // correctly hides their uncommitted inserts and restores their deleted tuples.
        undo_recover(&wal, &mut tx)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let engine = Engine::new();

        let mut preloaded_columnar: HashMap<String, oigrap_storage::ColumnarStore> = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("col") {
                    let stem = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    if let Ok(store) = oigrap_storage::ColumnarStore::load(&path) {
                        preloaded_columnar.insert(stem, store);
                    }
                }
            }
        }

        Ok(DbHandle {
            engine,
            pool,
            wal,
            tx,
            preloaded_columnar,
            _dir: DbDir::Persistent(dir.to_path_buf()),
        })
    }

    /// Execute a SQL string, returning a QueryResult or an error string.
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult, String> {
        self.engine
            .execute(sql, &mut self.pool, &mut self.wal, &mut self.tx)
            .map_err(|e| match e {
                SqlError::Parse(pe) => format!("syntax error: {}", pe.message),
                SqlError::Semantic(s) => s,
                SqlError::Execution(s) => s,
                SqlError::Storage(s) => format!("storage error: {:?}", s),
            })
    }

    /// Attach any preloaded columnar store for `table_name` into the catalog.
    /// Call this after a CREATE TABLE statement executes so the in-memory entry
    /// gets its columnar store restored from disk.
    #[allow(dead_code)]
    pub fn attach_columnar(&mut self, table_name: &str) {
        if let Some(store) = self.preloaded_columnar.remove(table_name) {
            if let Some(entry) = self.engine.catalog.get_mut(table_name) {
                entry.columnar = Some(store);
            }
        }
    }
}

// ── Value encoding for wire protocol ─────────────────────────────────────────

/// Map a Value to its PostgreSQL type OID (text format).
pub fn value_type_oid(v: &Value) -> u32 {
    match v {
        Value::Bool(_) => 16,
        Value::Int64(_) => 20,  // int8
        Value::Float64(_) => 701, // float8
        Value::Text(_) => 25,
        Value::Null => 25,
    }
}

/// Encode a Value as a text string for the wire protocol.
/// Returns (text_repr, is_null).
pub fn encode_value_text(v: &Value) -> (String, bool) {
    match v {
        Value::Null => (String::new(), true),
        Value::Bool(b) => (if *b { "t" } else { "f" }.to_string(), false),
        Value::Int64(n) => (n.to_string(), false),
        Value::Float64(f) => {
            // Match PostgreSQL's float formatting: no trailing zeros but show at least one decimal
            if f.is_nan() {
                ("NaN".to_string(), false)
            } else if f.is_infinite() {
                (if *f > 0.0 { "Infinity" } else { "-Infinity" }.to_string(), false)
            } else {
                (format!("{}", f), false)
            }
        }
        Value::Text(s) => (s.clone(), false),
    }
}

/// Substitute $1, $2, ... placeholders with literal SQL values.
pub fn substitute_params(sql: &str, params: &[Option<Vec<u8>>], formats: &[i16]) -> String {
    if params.is_empty() {
        return sql.to_string();
    }
    let mut result = String::with_capacity(sql.len() + params.len() * 8);
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                if let Ok(idx_str) = std::str::from_utf8(&bytes[start..j]) {
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        if idx >= 1 && idx <= params.len() {
                            let param = &params[idx - 1];
                            let fmt = formats.get(idx - 1).copied().unwrap_or(0);
                            match param {
                                None => result.push_str("NULL"),
                                Some(bytes) => {
                                    if fmt == 1 {
                                        // Binary format — treat as int8
                                        if bytes.len() == 8 {
                                            let n = i64::from_be_bytes(bytes[..8].try_into().unwrap());
                                            result.push_str(&n.to_string());
                                        } else {
                                            result.push_str("NULL");
                                        }
                                    } else {
                                        // Text format — escape as SQL string literal
                                        let s = String::from_utf8_lossy(bytes);
                                        // Try to determine if it looks like a number or boolean
                                        if s.parse::<i64>().is_ok() || s.parse::<f64>().is_ok()
                                            || s == "t" || s == "f" || s == "true" || s == "false"
                                        {
                                            result.push_str(&s);
                                        } else {
                                            result.push('\'');
                                            result.push_str(&s.replace('\'', "''"));
                                            result.push('\'');
                                        }
                                    }
                                }
                            }
                            i = j;
                            continue;
                        }
                    }
                }
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

// Keep an alias so existing imports of `HashMap` used indirectly still compile.
#[allow(dead_code)]
pub(crate) type _PreparedMap = HashMap<String, PreparedStmt>;

#[cfg(test)]
mod tests {
    use super::*;
    use oigrap_storage::ColumnarStore;

    #[test]
    fn test_columnar_files_loaded_on_open() {
        let dir = tempfile::tempdir().unwrap();

        // Save a dummy columnar store as "mytable.col"
        let store = ColumnarStore::new();
        store.save(&dir.path().join("mytable.col")).unwrap();

        // Open a DbHandle from that directory
        let handle = DbHandle::open(dir.path()).unwrap();

        // The preloaded map should contain "mytable"
        assert!(
            handle.preloaded_columnar.contains_key("mytable"),
            "expected mytable to be preloaded from disk"
        );
    }
}
