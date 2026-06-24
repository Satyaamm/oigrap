use crate::ast::DataType;
use crate::value::Value;
use oigrap_storage::{BTree, ColumnarStore, GinIndex, HeapFile, HnswIndex, PageId};
use std::collections::HashMap;

/// Simplified SQL type for the in-memory catalog.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlType {
    Boolean,
    Int64,
    Float64,
    Text,
}

// ---- Statistics for cost-based optimization ----

#[derive(Debug, Clone)]
pub struct ColumnStats {
    /// Fraction of rows that are NULL.
    pub null_fraction: f64,
    /// Number of distinct values.
    pub ndv: usize,
    /// Most-common values + frequency (top 10).
    pub mcv: Vec<(Value, f64)>,
    /// Histogram bucket boundaries (up to 50 buckets).
    pub hist_bounds: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct TableStats {
    pub row_count: usize,
    pub page_count: usize,
    pub columns: Vec<ColumnStats>,
}

impl SqlType {
    pub fn from_ast(dt: &DataType) -> Option<Self> {
        match dt {
            DataType::Boolean => Some(SqlType::Boolean),
            DataType::Int16 | DataType::Int32 | DataType::Int64 => Some(SqlType::Int64),
            DataType::Float32 | DataType::Float64 => Some(SqlType::Float64),
            DataType::Text | DataType::Varchar(_) | DataType::Char(_) => Some(SqlType::Text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ColumnSchema {
    pub name: String,
    pub sql_type: SqlType,
    pub nullable: bool,
    pub primary_key: bool,
}

pub struct TableEntry {
    pub table_id: u32,
    pub columns: Vec<ColumnSchema>,
    pub pk_col_idx: Option<usize>,
    pub heap: HeapFile,
    pub pk_index: Option<BTree>,
    pub pk_index_meta: Option<PageId>,
    /// Per-table statistics collected by ANALYZE.
    pub stats: Option<TableStats>,
    /// Columnar store for dual-write tables (STORAGE COLUMNAR).
    pub columnar: Option<ColumnarStore>,
    /// HNSW vector index (built by CREATE VECTOR INDEX).
    pub vector_index: Option<HnswIndex>,
    /// GIN inverted index (built by CREATE GIN INDEX).
    pub gin_index: Option<GinIndex>,
    /// Column name for which the GIN index was built.
    pub gin_column: Option<String>,
}

pub struct Catalog {
    tables: HashMap<String, TableEntry>,
    next_table_id: u32,
}

impl Catalog {
    pub fn new() -> Self {
        Catalog { tables: HashMap::new(), next_table_id: 1 }
    }

    pub fn create_table(
        &mut self,
        name: String,
        columns: Vec<ColumnSchema>,
        pk_col_idx: Option<usize>,
        heap: HeapFile,
        pk_index: Option<BTree>,
        pk_index_meta: Option<PageId>,
    ) {
        let table_id = self.next_table_id;
        self.next_table_id += 1;
        self.tables.insert(
            name.to_lowercase(),
            TableEntry {
                table_id,
                columns,
                pk_col_idx,
                heap,
                pk_index,
                pk_index_meta,
                stats: None,
                columnar: None,
                vector_index: None,
                gin_index: None,
                gin_column: None,
            },
        );
    }

    pub fn get(&self, name: &str) -> Option<&TableEntry> {
        self.tables.get(&name.to_lowercase())
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut TableEntry> {
        self.tables.get_mut(&name.to_lowercase())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tables.contains_key(&name.to_lowercase())
    }

    pub fn next_id(&self) -> u32 {
        self.next_table_id
    }

    /// Iterate over all (name, entry) pairs in the catalog.
    pub fn tables(&self) -> impl Iterator<Item = (&String, &TableEntry)> {
        self.tables.iter()
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}
