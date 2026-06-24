/// Columnar storage with Run-Length Encoding (RLE) and Zone Maps.
/// A Value type for columnar storage (simplified, mirrors sql::value::Value).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int64(i64),
    Float64(f64),
    Text(String),
    Bool(bool),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Value::Int64(a), Value::Int64(b)) => a.partial_cmp(b),
            (Value::Float64(a), Value::Float64(b)) => a.partial_cmp(b),
            (Value::Text(a), Value::Text(b)) => a.partial_cmp(b),
            (Value::Bool(a), Value::Bool(b)) => a.partial_cmp(b),
            (Value::Null, Value::Null) => Some(std::cmp::Ordering::Equal),
            (Value::Null, _) => Some(std::cmp::Ordering::Less),
            (_, Value::Null) => Some(std::cmp::Ordering::Greater),
            _ => None,
        }
    }
}

/// A column stored with Run-Length Encoding.
/// Each run is (value, count).
#[derive(Debug, Clone)]
pub struct RleColumn {
    pub name: String,
    pub runs: Vec<(Value, u32)>,
    pub row_count: usize,
}

impl RleColumn {
    pub fn new(name: String) -> Self {
        RleColumn { name, runs: Vec::new(), row_count: 0 }
    }

    /// Append a single value.
    pub fn push(&mut self, val: Value) {
        self.row_count += 1;
        if let Some(last) = self.runs.last_mut() {
            if last.0 == val {
                last.1 += 1;
                return;
            }
        }
        self.runs.push((val, 1));
    }

    /// Materialize all values.
    pub fn values(&self) -> Vec<Value> {
        let mut out = Vec::with_capacity(self.row_count);
        for (val, count) in &self.runs {
            for _ in 0..*count {
                out.push(val.clone());
            }
        }
        out
    }

    /// Get the value at a specific row index.
    pub fn get(&self, row_idx: usize) -> Option<Value> {
        let mut remaining = row_idx;
        for (val, count) in &self.runs {
            if remaining < *count as usize {
                return Some(val.clone());
            }
            remaining -= *count as usize;
        }
        None
    }

    /// Number of RLE runs (compression metric).
    pub fn run_count(&self) -> usize {
        self.runs.len()
    }
}

/// Dictionary-encoded column: each row stored as a u32 index into a dictionary
/// of unique values. Provides good compression when cardinality is low.
pub struct DictColumn {
    pub name: String,
    /// Unique values in insertion order.
    pub dict: Vec<Value>,
    /// One code per row: index into `dict`.
    pub codes: Vec<u32>,
    pub row_count: usize,
}

impl DictColumn {
    pub fn new(name: &str) -> Self {
        DictColumn {
            name: name.to_string(),
            dict: Vec::new(),
            codes: Vec::new(),
            row_count: 0,
        }
    }

    /// Append a value, adding it to the dictionary if not already present.
    pub fn append(&mut self, v: Value) {
        let idx = match self.dict.iter().position(|d| d == &v) {
            Some(pos) => pos as u32,
            None => {
                let pos = self.dict.len() as u32;
                self.dict.push(v);
                pos
            }
        };
        self.codes.push(idx);
        self.row_count += 1;
    }

    /// Retrieve the value at a given row index.
    pub fn get(&self, row: usize) -> &Value {
        let code = self.codes[row] as usize;
        &self.dict[code]
    }

    /// Number of distinct values in the dictionary.
    pub fn dict_size(&self) -> usize {
        self.dict.len()
    }

    /// Compression ratio: row_count / dict_size. Higher is better.
    /// Returns 0.0 if the dictionary is empty.
    pub fn compression_ratio(&self) -> f64 {
        if self.dict.is_empty() {
            0.0
        } else {
            self.row_count as f64 / self.dict_size() as f64
        }
    }
}

/// Delta-encoded integer column: stores first value + differences between consecutive values.
/// Efficient for monotonically increasing or slowly changing integer sequences.
pub struct DeltaColumn {
    pub name: String,
    pub first_value: i64,
    pub deltas: Vec<i32>,   // delta[i] = value[i+1] - value[i], clamped to i32
    pub row_count: usize,
    last_value: i64,
}

impl DeltaColumn {
    pub fn new(name: &str) -> Self {
        DeltaColumn {
            name: name.to_string(),
            first_value: 0,
            deltas: Vec::new(),
            row_count: 0,
            last_value: 0,
        }
    }

    pub fn append(&mut self, v: i64) {
        if self.row_count == 0 {
            self.first_value = v;
            self.last_value = v;
        } else {
            let delta = (v - self.last_value) as i32;
            self.deltas.push(delta);
            self.last_value = v;
        }
        self.row_count += 1;
    }

    pub fn get(&self, row: usize) -> i64 {
        if row == 0 || self.row_count == 0 {
            return self.first_value;
        }
        let limit = row.min(self.deltas.len());
        let mut val = self.first_value;
        for i in 0..limit {
            val += self.deltas[i] as i64;
        }
        val
    }

    pub fn to_rle_column(&self) -> RleColumn {
        let mut rle = RleColumn::new(self.name.clone());
        for i in 0..self.row_count {
            rle.push(Value::Int64(self.get(i)));
        }
        rle
    }

    pub fn compression_ratio(&self) -> f64 {
        if self.row_count <= 1 {
            return 1.0;
        }
        (self.row_count as f64 * 8.0) / (8.0 + (self.row_count - 1) as f64 * 4.0)
    }

    pub fn row_count(&self) -> usize {
        self.row_count
    }
}

/// Convert an RleColumn to a DeltaColumn. Returns None if the column contains
/// any non-integer values or nulls.
pub fn from_rle_to_delta(rle: &RleColumn) -> Option<DeltaColumn> {
    let mut delta = DeltaColumn::new(&rle.name);
    for val in rle.values() {
        match val {
            Value::Int64(n) => delta.append(n),
            _ => return None,
        }
    }
    Some(delta)
}

/// Convert an RleColumn to DictColumn form.
pub fn to_dict_column(rle: &RleColumn) -> DictColumn {
    let mut dict = DictColumn::new(&rle.name);
    for v in rle.values() {
        dict.append(v);
    }
    dict
}

/// Convert a DictColumn back to RleColumn form.
pub fn from_dict_column(dict: &DictColumn) -> RleColumn {
    let mut rle = RleColumn::new(dict.name.clone());
    for code in &dict.codes {
        rle.push(dict.dict[*code as usize].clone());
    }
    rle
}

/// Zone map: min/max statistics over a range of rows.
#[derive(Debug, Clone)]
pub struct ZoneMap {
    pub row_offset: usize,
    pub row_count: usize,
    pub min: Value,
    pub max: Value,
}

impl ZoneMap {
    /// Check if a filter `col > bound` can be pruned (i.e. max <= bound means all rows fail).
    pub fn can_prune_gt(&self, bound: &Value) -> bool {
        // If max <= bound, no row passes col > bound
        if let Some(ord) = self.max.partial_cmp(bound) {
            ord != std::cmp::Ordering::Greater
        } else {
            false
        }
    }

    /// Check if a filter `col < bound` can be pruned (i.e. min >= bound means all rows fail).
    pub fn can_prune_lt(&self, bound: &Value) -> bool {
        // If min >= bound, no row passes col < bound
        if let Some(ord) = self.min.partial_cmp(bound) {
            ord != std::cmp::Ordering::Less
        } else {
            false
        }
    }
}

/// Zone size: number of rows per zone.
pub const ZONE_SIZE: usize = 1000;

/// Columnar store for one table. Each column stored separately with RLE.
pub struct ColumnarStore {
    pub columns: Vec<RleColumn>,
    pub zone_maps: Vec<Vec<ZoneMap>>,  // zone_maps[col_idx][zone_idx]
    pub row_count: usize,
}

impl ColumnarStore {
    pub fn new() -> Self {
        ColumnarStore {
            columns: Vec::new(),
            zone_maps: Vec::new(),
            row_count: 0,
        }
    }

    /// Append a batch of rows. col_names define the column order.
    pub fn insert_rows(&mut self, col_names: &[String], rows: &[Vec<Value>]) {
        // Initialize columns if needed
        if self.columns.is_empty() {
            for name in col_names {
                self.columns.push(RleColumn::new(name.clone()));
                self.zone_maps.push(Vec::new());
            }
        }

        for row in rows {
            for (col_idx, val) in row.iter().enumerate() {
                if col_idx < self.columns.len() {
                    self.columns[col_idx].push(val.clone());
                }
            }
            self.row_count += 1;

            // Rebuild zone maps every ZONE_SIZE rows
            if self.row_count.is_multiple_of(ZONE_SIZE) {
                self.build_zone_maps();
            }
        }
    }

    /// Scan all rows and return as a Vec of rows.
    pub fn scan(&self) -> Vec<Vec<Value>> {
        if self.columns.is_empty() || self.row_count == 0 {
            return vec![];
        }
        let mut rows = Vec::with_capacity(self.row_count);
        for row_idx in 0..self.row_count {
            let row: Vec<Value> = self.columns.iter().map(|col| {
                col.get(row_idx).unwrap_or(Value::Null)
            }).collect();
            rows.push(row);
        }
        rows
    }

    /// Scan a single column by index, returning all its values.
    pub fn scan_column(&self, col_idx: usize) -> Vec<Value> {
        if col_idx < self.columns.len() {
            self.columns[col_idx].values()
        } else {
            vec![]
        }
    }

    /// Build zone maps for each column (one zone per ZONE_SIZE rows).
    pub fn build_zone_maps(&mut self) {
        let row_count = self.row_count;
        for col_idx in 0..self.columns.len() {
            let values = self.columns[col_idx].values();
            let mut zones = Vec::new();
            let mut offset = 0;
            while offset < row_count {
                let end = (offset + ZONE_SIZE).min(row_count);
                let zone_vals = &values[offset..end];

                let mut min_val: Option<Value> = None;
                let mut max_val: Option<Value> = None;

                for v in zone_vals {
                    if v.is_null() { continue; }
                    min_val = Some(match min_val.take() {
                        None => v.clone(),
                        Some(cur) => if v.partial_cmp(&cur).map(|o| o.is_lt()).unwrap_or(false) {
                            v.clone()
                        } else { cur },
                    });
                    max_val = Some(match max_val.take() {
                        None => v.clone(),
                        Some(cur) => if v.partial_cmp(&cur).map(|o| o.is_gt()).unwrap_or(false) {
                            v.clone()
                        } else { cur },
                    });
                }

                zones.push(ZoneMap {
                    row_offset: offset,
                    row_count: end - offset,
                    min: min_val.unwrap_or(Value::Null),
                    max: max_val.unwrap_or(Value::Null),
                });
                offset += ZONE_SIZE;
            }
            self.zone_maps[col_idx] = zones;
        }
    }

    /// Check if ANY zone for col_idx can be pruned given a bound.
    /// is_gt=true means query is `col > bound`, is_gt=false means `col < bound`.
    /// Returns true if ALL zones can be pruned (i.e. entire column definitely doesn't match).
    pub fn can_prune(&self, col_idx: usize, bound: &Value, is_gt: bool) -> bool {
        if col_idx >= self.zone_maps.len() || self.zone_maps[col_idx].is_empty() {
            return false;
        }
        self.zone_maps[col_idx].iter().all(|zm| {
            if is_gt {
                zm.can_prune_gt(bound)
            } else {
                zm.can_prune_lt(bound)
            }
        })
    }

    /// Column name lookup.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }
}

impl Default for ColumnarStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ColumnarStore {
    /// Serialize to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        // magic
        buf.extend_from_slice(b"COL\0");
        // version
        buf.push(1u8);
        // column_count
        buf.extend_from_slice(&(self.columns.len() as u32).to_le_bytes());
        // row_count
        buf.extend_from_slice(&(self.row_count as u64).to_le_bytes());
        // columns
        for col in &self.columns {
            let name_bytes = col.name.as_bytes();
            buf.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(name_bytes);
            buf.extend_from_slice(&(col.runs.len() as u32).to_le_bytes());
            for (val, run_len) in &col.runs {
                match val {
                    Value::Null => {
                        buf.push(0u8);
                    }
                    Value::Int64(v) => {
                        buf.push(1u8);
                        buf.extend_from_slice(&v.to_le_bytes());
                    }
                    Value::Float64(v) => {
                        buf.push(2u8);
                        buf.extend_from_slice(&v.to_le_bytes());
                    }
                    Value::Text(s) => {
                        buf.push(3u8);
                        let sb = s.as_bytes();
                        buf.extend_from_slice(&(sb.len() as u32).to_le_bytes());
                        buf.extend_from_slice(sb);
                    }
                    Value::Bool(b) => {
                        buf.push(4u8);
                        buf.push(if *b { 1u8 } else { 0u8 });
                    }
                }
                buf.extend_from_slice(&run_len.to_le_bytes());
            }
        }
        buf
    }

    /// Deserialize from bytes. Returns Err if format is wrong.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let mut pos = 0usize;

        macro_rules! need {
            ($n:expr) => {
                if pos + $n > data.len() {
                    return Err(format!("truncated at offset {}", pos));
                }
            };
        }
        macro_rules! read_u8 {
            () => {{
                need!(1);
                let v = data[pos];
                pos += 1;
                v
            }};
        }
        macro_rules! read_u32 {
            () => {{
                need!(4);
                let v = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
                pos += 4;
                v
            }};
        }
        macro_rules! read_u64 {
            () => {{
                need!(8);
                let v = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
                pos += 8;
                v
            }};
        }
        macro_rules! read_i64 {
            () => {{
                need!(8);
                let v = i64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
                pos += 8;
                v
            }};
        }
        macro_rules! read_f64 {
            () => {{
                need!(8);
                let v = f64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
                pos += 8;
                v
            }};
        }

        // magic
        need!(4);
        if &data[pos..pos + 4] != b"COL\0" {
            return Err("bad magic".to_string());
        }
        pos += 4;

        let version = read_u8!();
        if version != 1 {
            return Err(format!("unsupported version {}", version));
        }

        let column_count = read_u32!() as usize;
        let row_count = read_u64!() as usize;

        let mut columns = Vec::with_capacity(column_count);
        let zone_maps = vec![Vec::new(); column_count];

        for _ in 0..column_count {
            let name_len = read_u32!() as usize;
            need!(name_len);
            let name = std::str::from_utf8(&data[pos..pos + name_len])
                .map_err(|e| format!("invalid column name UTF-8: {}", e))?
                .to_string();
            pos += name_len;

            let run_count = read_u32!() as usize;
            let mut runs = Vec::with_capacity(run_count);
            let mut col_row_count = 0usize;

            for _ in 0..run_count {
                let tag = read_u8!();
                let val = match tag {
                    0 => Value::Null,
                    1 => Value::Int64(read_i64!()),
                    2 => Value::Float64(read_f64!()),
                    3 => {
                        let slen = read_u32!() as usize;
                        need!(slen);
                        let s = std::str::from_utf8(&data[pos..pos + slen])
                            .map_err(|e| format!("invalid text UTF-8: {}", e))?
                            .to_string();
                        pos += slen;
                        Value::Text(s)
                    }
                    4 => Value::Bool(read_u8!() != 0),
                    other => return Err(format!("unknown value tag {}", other)),
                };
                let run_len = read_u32!();
                col_row_count += run_len as usize;
                runs.push((val, run_len));
            }

            columns.push(RleColumn { name, runs, row_count: col_row_count });
        }

        Ok(ColumnarStore { columns, zone_maps, row_count })
    }

    /// Save to a file path.
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let bytes = self.to_bytes();
        std::fs::write(path, &bytes)
    }

    /// Load from a file path.
    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::from_bytes(&bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_columnar_rle() {
        let mut store = ColumnarStore::new();
        let col_names = vec!["category".to_string(), "value".to_string()];

        // Insert rows with repeated values for good RLE compression
        let mut rows = Vec::new();
        for i in 0..100usize {
            rows.push(vec![
                Value::Text(if i < 50 { "A".to_string() } else { "B".to_string() }),
                Value::Int64(i as i64),
            ]);
        }
        store.insert_rows(&col_names, &rows);

        assert_eq!(store.row_count, 100);

        // Verify RLE compression: "category" column should have only 2 runs
        let cat_col = &store.columns[0];
        assert_eq!(cat_col.run_count(), 2, "expected 2 RLE runs for category column");

        // Verify scan correctness
        let scanned = store.scan();
        assert_eq!(scanned.len(), 100);
        assert_eq!(scanned[0][0], Value::Text("A".to_string()));
        assert_eq!(scanned[49][0], Value::Text("A".to_string()));
        assert_eq!(scanned[50][0], Value::Text("B".to_string()));
        assert_eq!(scanned[99][0], Value::Text("B".to_string()));

        // Verify column scan
        let val_col = store.scan_column(1);
        assert_eq!(val_col.len(), 100);
        for (i, v) in val_col.iter().enumerate() {
            assert_eq!(*v, Value::Int64(i as i64));
        }
    }

    #[test]
    fn test_zone_map_pruning() {
        let mut store = ColumnarStore::new();
        let col_names = vec!["n".to_string()];

        // Insert 2000 rows: first 1000 have values 0..999, second 1000 have 1000..1999
        let mut rows = Vec::new();
        for i in 0..2000i64 {
            rows.push(vec![Value::Int64(i)]);
        }
        store.insert_rows(&col_names, &rows);
        store.build_zone_maps();

        // Zone 0: n in [0, 999], Zone 1: n in [1000, 1999]

        // Query: n > 1999 -> all zones should be prunable (max of both zones <= 1999? No, zone 1 max = 1999)
        // Actually max=1999 and bound=1999: max <= bound is true (1999 <= 1999), so prunable
        let prune = store.can_prune(0, &Value::Int64(1999), true);
        assert!(prune, "should prune: no row has n > 1999");

        // Query: n > 500 -> zone 0 max=999 > 500, so can't prune zone 0
        let prune2 = store.can_prune(0, &Value::Int64(500), true);
        assert!(!prune2, "should NOT prune: zone 0 has rows with n > 500");

        // Query: n < 0 -> min of zone 0 = 0, 0 >= 0 so prunable
        let prune3 = store.can_prune(0, &Value::Int64(0), false);
        assert!(prune3, "should prune: no row has n < 0");

        // Query: n < 500 -> zone 0 min=0 < 500 so NOT prunable
        let prune4 = store.can_prune(0, &Value::Int64(500), false);
        assert!(!prune4, "should NOT prune: zone 0 has rows with n < 500");
    }

    #[test]
    fn test_rle_single_value() {
        let mut col = RleColumn::new("x".to_string());
        for _ in 0..1000 {
            col.push(Value::Int64(42));
        }
        assert_eq!(col.run_count(), 1, "all same values should produce 1 RLE run");
        assert_eq!(col.row_count, 1000);
        let vals = col.values();
        assert!(vals.iter().all(|v| *v == Value::Int64(42)));
    }

    #[test]
    fn test_columnar_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("col.bin");

        let mut store = ColumnarStore::new();
        let col_names = vec!["id".to_string(), "score".to_string(), "label".to_string(), "active".to_string()];
        let mut rows = Vec::new();
        for i in 0..1000usize {
            rows.push(vec![
                Value::Int64(i as i64),
                Value::Float64(i as f64 * 0.5),
                Value::Text(if i % 3 == 0 { "A".to_string() } else { "B".to_string() }),
                Value::Bool(i % 2 == 0),
            ]);
        }
        store.insert_rows(&col_names, &rows);

        store.save(&path).unwrap();
        let loaded = ColumnarStore::load(&path).unwrap();

        assert_eq!(loaded.row_count, store.row_count);
        assert_eq!(loaded.columns.len(), store.columns.len());

        let scanned = loaded.scan();
        assert_eq!(scanned.len(), 1000);
        for (i, row) in scanned.iter().enumerate() {
            assert_eq!(row[0], Value::Int64(i as i64));
            assert_eq!(row[1], Value::Float64(i as f64 * 0.5));
            let expected_label = if i % 3 == 0 { "A" } else { "B" };
            assert_eq!(row[2], Value::Text(expected_label.to_string()));
            assert_eq!(row[3], Value::Bool(i % 2 == 0));
        }
    }

    #[test]
    fn test_columnar_persistence_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("col_empty.bin");

        let store = ColumnarStore::new();
        store.save(&path).unwrap();

        let loaded = ColumnarStore::load(&path).unwrap();
        assert_eq!(loaded.row_count, 0);
        assert_eq!(loaded.columns.len(), 0);
        assert!(loaded.scan().is_empty());
    }

    #[test]
    fn test_dict_column_basic() {
        let mut col = DictColumn::new("label");
        let vals = [Value::Text("X".to_string()), Value::Text("Y".to_string()), Value::Text("Z".to_string())];
        for _ in 0..100 {
            for v in &vals {
                col.append(v.clone());
            }
        }
        // 300 rows total, 3 distinct values
        assert_eq!(col.row_count, 300);
        assert_eq!(col.dict_size(), 3);
        // Verify get() returns correct values
        assert_eq!(col.get(0), &Value::Text("X".to_string()));
        assert_eq!(col.get(1), &Value::Text("Y".to_string()));
        assert_eq!(col.get(2), &Value::Text("Z".to_string()));
        // Pattern repeats: index 3=X, 4=Y, 5=Z, 6=X, 7=Y, 8=Z, 9=X
        assert_eq!(col.get(9), &Value::Text("X".to_string()));
        // 299 = 99*3 + 2 => Z
        assert_eq!(col.get(299), &Value::Text("Z".to_string()));
    }

    #[test]
    fn test_dict_roundtrip() {
        // Build an RleColumn with repeating values
        let mut rle = RleColumn::new("color".to_string());
        let pattern = ["red", "green", "blue", "red", "red"];
        for _ in 0..20 {
            for s in &pattern {
                rle.push(Value::Text(s.to_string()));
            }
        }
        let original_values = rle.values();

        // Convert to DictColumn and back
        let dict = to_dict_column(&rle);
        let restored = from_dict_column(&dict);

        assert_eq!(restored.row_count, rle.row_count);
        let restored_values = restored.values();
        assert_eq!(restored_values, original_values, "roundtrip must preserve all values");
    }

    #[test]
    fn test_delta_column_monotonic() {
        let mut col = DeltaColumn::new("seq");
        for i in 0..100i64 {
            col.append(i);
        }
        assert_eq!(col.get(50), 50);
        assert_eq!(col.row_count(), 100);
        // All deltas should be 1
        assert!(col.deltas.iter().all(|&d| d == 1));
    }

    #[test]
    fn test_delta_column_to_rle_roundtrip() {
        let mut col = DeltaColumn::new("vals");
        for &v in &[10i64, 20, 10, 30] {
            col.append(v);
        }
        let rle = col.to_rle_column();
        let values = rle.values();
        assert_eq!(values.len(), 4);
        assert_eq!(values[0], Value::Int64(10));
        assert_eq!(values[1], Value::Int64(20));
        assert_eq!(values[2], Value::Int64(10));
        assert_eq!(values[3], Value::Int64(30));
    }

    #[test]
    fn test_delta_compression_ratio() {
        let mut col = DeltaColumn::new("mono");
        for i in 0..1000i64 {
            col.append(i);
        }
        let ratio = col.compression_ratio();
        // Expected: 1000*8 / (8 + 999*4) = 8000 / 4004 ≈ 1.998...
        assert!((ratio - 2.0).abs() < 0.01, "ratio was {}", ratio);
    }

    #[test]
    fn test_dict_compression_ratio() {
        let mut col = DictColumn::new("bucket");
        let distinct = ["a", "b", "c", "d", "e"];
        for _ in 0..200 {
            for s in &distinct {
                col.append(Value::Text(s.to_string()));
            }
        }
        assert_eq!(col.row_count, 1000);
        assert_eq!(col.dict_size(), 5);
        assert!((col.compression_ratio() - 200.0).abs() < f64::EPSILON);
    }
}
