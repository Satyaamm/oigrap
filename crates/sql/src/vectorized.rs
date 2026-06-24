//! Vectorized (batch / columnar) execution path for aggregate queries.
//!
//! This module provides a batch evaluation layer that operates over columnar data
//! directly, avoiding row-at-a-time overhead for simple aggregate queries.
//! It does NOT replace the existing row-at-a-time executor; instead, executor.rs
//! may call `vectorized_aggregate` when the table has a columnar store and the
//! query is a simple aggregate.

use crate::ast::{BinOp, Expr, SelectColumn};
use crate::catalog::ColumnSchema;
use crate::error::{Result, SqlError};
use crate::executor::eval_literal;
use crate::value::Value;
use oigrap_storage::ColumnarStore;
use oigrap_storage::columnar::ZONE_SIZE;

/// A column-major batch of values.
pub struct ColumnBatch {
    /// columns\[col_idx\]\[row_idx\]
    pub columns: Vec<Vec<Value>>,
    pub row_count: usize,
}

impl ColumnBatch {
    /// Build a ColumnBatch from a ColumnarStore by materializing all columns.
    pub fn from_columnar(store: &ColumnarStore) -> Self {
        let row_count = store.row_count;
        let columns: Vec<Vec<Value>> = (0..store.columns.len())
            .map(|i| {
                store
                    .scan_column(i)
                    .into_iter()
                    .map(columnar_to_sql)
                    .collect()
            })
            .collect();
        ColumnBatch { columns, row_count }
    }

    /// Apply a boolean filter mask. Returns a new batch containing only rows where mask[i]=true.
    pub fn filter(&self, mask: &[bool]) -> Self {
        assert_eq!(mask.len(), self.row_count);
        let row_count = mask.iter().filter(|&&b| b).count();
        let columns = self
            .columns
            .iter()
            .map(|col| {
                col.iter()
                    .zip(mask.iter())
                    .filter_map(|(v, &keep)| if keep { Some(v.clone()) } else { None })
                    .collect()
            })
            .collect();
        ColumnBatch { columns, row_count }
    }

    /// Evaluate a simple filter expression over all rows, returning a boolean mask.
    ///
    /// Supports binary comparisons against literals (Eq, NotEq, Lt, Gt, LtEq, GtEq).
    /// For unsupported expressions the mask defaults to all-true.
    pub fn eval_filter(&self, expr: &Expr, schema: &[ColumnSchema]) -> Vec<bool> {
        match expr {
            Expr::BinaryOp { op, left, right } => {
                // Try col op literal
                if let (Some(col_idx), Ok(rhs)) = (
                    col_ref_idx(left, schema, &self.columns),
                    eval_literal(right),
                ) {
                    return self.columns[col_idx]
                        .iter()
                        .map(|v| cmp_op(op, v, &rhs))
                        .collect();
                }
                // Try literal op col
                if let (Ok(lhs), Some(col_idx)) = (
                    eval_literal(left),
                    col_ref_idx(right, schema, &self.columns),
                ) {
                    let flipped = flip_op(op);
                    return self.columns[col_idx]
                        .iter()
                        .map(|v| cmp_op(&flipped, v, &lhs))
                        .collect();
                }
                // Fallback: keep all rows
                vec![true; self.row_count]
            }
            _ => vec![true; self.row_count],
        }
    }

    /// Compute a single aggregate (COUNT/SUM/AVG/MIN/MAX) over a column.
    pub fn aggregate_column(&self, col_idx: usize, agg: &str) -> Value {
        let col = match self.columns.get(col_idx) {
            Some(c) => c,
            None => return Value::Null,
        };
        let upper = agg.to_uppercase();
        match upper.as_str() {
            "COUNT" => Value::Int64(col.iter().filter(|v| !v.is_null()).count() as i64),
            "SUM" => {
                let mut sum_i: Option<i64> = None;
                let mut sum_f: Option<f64> = None;
                for v in col {
                    match v {
                        Value::Int64(n) => sum_i = Some(sum_i.unwrap_or(0).wrapping_add(*n)),
                        Value::Float64(f) => sum_f = Some(sum_f.unwrap_or(0.0) + f),
                        _ => {}
                    }
                }
                if let Some(f) = sum_f {
                    Value::Float64(f + sum_i.unwrap_or(0) as f64)
                } else {
                    sum_i.map(Value::Int64).unwrap_or(Value::Null)
                }
            }
            "AVG" => {
                let mut sum = 0.0f64;
                let mut count = 0usize;
                for v in col {
                    match v {
                        Value::Int64(n) => { sum += *n as f64; count += 1; }
                        Value::Float64(f) => { sum += f; count += 1; }
                        _ => {}
                    }
                }
                if count == 0 { Value::Null } else { Value::Float64(sum / count as f64) }
            }
            "MIN" => {
                let mut min_val: Option<Value> = None;
                for v in col {
                    if v.is_null() { continue; }
                    min_val = Some(match min_val.take() {
                        None => v.clone(),
                        Some(cur) => {
                            if v.partial_cmp(&cur).map(|o| o.is_lt()).unwrap_or(false) {
                                v.clone()
                            } else {
                                cur
                            }
                        }
                    });
                }
                min_val.unwrap_or(Value::Null)
            }
            "MAX" => {
                let mut max_val: Option<Value> = None;
                for v in col {
                    if v.is_null() { continue; }
                    max_val = Some(match max_val.take() {
                        None => v.clone(),
                        Some(cur) => {
                            if v.partial_cmp(&cur).map(|o| o.is_gt()).unwrap_or(false) {
                                v.clone()
                            } else {
                                cur
                            }
                        }
                    });
                }
                max_val.unwrap_or(Value::Null)
            }
            _ => Value::Null,
        }
    }
}

/// Execute a vectorized aggregate query over a ColumnarStore.
///
/// Returns `(column_names, result_rows)` on success.
/// Only handles ungrouped aggregates (no GROUP BY) for now.
/// The caller should fall through to the row-at-a-time path for GROUP BY.
pub fn vectorized_aggregate(
    store: &ColumnarStore,
    schema: &[ColumnSchema],
    select_cols: &[SelectColumn],
    where_clause: &Option<Expr>,
    group_by: &[Expr],
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    // Build the initial batch from the columnar store, using zone-map pruning when possible.
    let batch = if let Some(expr) = where_clause {
        let surviving = prune_with_zone_maps(store, schema, expr);
        // If zone maps could prune some zones, build a reduced batch; otherwise full scan.
        let n_zones = store.row_count.div_ceil(ZONE_SIZE);
        if surviving.len() < n_zones {
            batch_from_zones(store, &surviving)
        } else {
            ColumnBatch::from_columnar(store)
        }
    } else {
        ColumnBatch::from_columnar(store)
    };

    // Apply WHERE filter if present
    let filtered = if let Some(expr) = where_clause {
        let mask = batch.eval_filter(expr, schema);
        batch.filter(&mask)
    } else {
        batch
    };

    if !group_by.is_empty() {
        return vectorized_aggregate_grouped(schema, select_cols, &filtered, group_by);
    }

    let mut col_names: Vec<String> = Vec::new();
    let mut result_row: Vec<Value> = Vec::new();

    for (col_idx_out, sc) in select_cols.iter().enumerate() {
        match sc {
            SelectColumn::Star => {
                return Err(SqlError::Execution(
                    "vectorized_aggregate: SELECT * not valid in aggregate context".into(),
                ));
            }
            SelectColumn::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| format!("col{}", col_idx_out));
                col_names.push(name);
                let val = eval_agg_expr_vectorized(expr, schema, &filtered)?;
                result_row.push(val);
            }
        }
    }

    Ok((col_names, vec![result_row]))
}

/// Evaluate a group-by expression for a single row (indexed into the batch columns).
fn eval_group_expr(
    expr: &Expr,
    schema: &[ColumnSchema],
    batch: &ColumnBatch,
    row_idx: usize,
) -> Value {
    match expr {
        Expr::ColumnRef { column, .. } => {
            let col_lower = column.to_lowercase();
            let col_idx = schema.iter().position(|c| {
                let n = c.name.to_lowercase();
                n == col_lower || n.ends_with(&format!(".{}", col_lower))
            });
            match col_idx {
                Some(idx) => batch.columns.get(idx)
                    .and_then(|col| col.get(row_idx))
                    .cloned()
                    .unwrap_or(Value::Null),
                None => Value::Null,
            }
        }
        Expr::IntLit(n) => Value::Int64(*n),
        Expr::FloatLit(f) => Value::Float64(*f),
        Expr::StrLit(s) => Value::Text(s.clone()),
        Expr::BoolLit(b) => Value::Bool(*b),
        Expr::Null => Value::Null,
        _ => Value::Null,
    }
}

/// Grouped aggregate path: one output row per distinct group-key.
fn vectorized_aggregate_grouped(
    schema: &[ColumnSchema],
    select_cols: &[SelectColumn],
    batch: &ColumnBatch,
    group_by: &[Expr],
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    // Accumulate groups: Vec<(serialised_key, group_key_values, rows_in_group)>
    // We use Vec instead of HashMap to avoid requiring Hash on Value.
    // For small cardinality GROUP BY this is fine.
    let mut groups: Vec<(String, Vec<Value>, Vec<Vec<Value>>)> = Vec::new();

    for row_idx in 0..batch.row_count {
        // Build group key for this row.
        let key_vals: Vec<Value> = group_by
            .iter()
            .map(|expr| eval_group_expr(expr, schema, batch, row_idx))
            .collect();
        let key_str: String = key_vals.iter().map(|v| format!("{:?}", v)).collect::<Vec<_>>().join(",");

        // Materialise the full row for this row_idx.
        let row_data: Vec<Value> = batch.columns.iter()
            .map(|col| col.get(row_idx).cloned().unwrap_or(Value::Null))
            .collect();

        // Find or create group.
        if let Some(g) = groups.iter_mut().find(|(k, _, _)| k == &key_str) {
            g.2.push(row_data);
        } else {
            groups.push((key_str, key_vals, vec![row_data]));
        }
    }

    // Build column names from select_cols.
    let mut col_names: Vec<String> = Vec::new();
    for (col_idx_out, sc) in select_cols.iter().enumerate() {
        match sc {
            SelectColumn::Star => {
                return Err(SqlError::Execution(
                    "vectorized_aggregate: SELECT * not valid in aggregate context".into(),
                ));
            }
            SelectColumn::Expr { alias, .. } => {
                let name = alias.clone().unwrap_or_else(|| format!("col{}", col_idx_out));
                col_names.push(name);
            }
        }
    }

    // For each group, compute the select expressions.
    let mut result_rows: Vec<Vec<Value>> = Vec::with_capacity(groups.len());

    for (_key_str, _key_vals, group_rows) in &groups {
        // Build a mini ColumnBatch for this group.
        let group_batch = rows_to_batch(group_rows, batch.columns.len());

        let mut out_row: Vec<Value> = Vec::with_capacity(select_cols.len());
        for sc in select_cols {
            match sc {
                SelectColumn::Star => {
                    return Err(SqlError::Execution(
                        "vectorized_aggregate: SELECT * not valid in aggregate context".into(),
                    ));
                }
                SelectColumn::Expr { expr, .. } => {
                    let val = eval_agg_expr_vectorized(expr, schema, &group_batch)?;
                    out_row.push(val);
                }
            }
        }
        result_rows.push(out_row);
    }

    Ok((col_names, result_rows))
}

/// Convert a slice of rows (each row = Vec<Value> over all columns) into a ColumnBatch.
fn rows_to_batch(rows: &[Vec<Value>], col_count: usize) -> ColumnBatch {
    let row_count = rows.len();
    let mut columns: Vec<Vec<Value>> = vec![Vec::with_capacity(row_count); col_count];
    for row in rows {
        for (ci, val) in row.iter().enumerate() {
            if ci < col_count {
                columns[ci].push(val.clone());
            }
        }
    }
    ColumnBatch { columns, row_count }
}

fn eval_agg_expr_vectorized(
    expr: &Expr,
    schema: &[ColumnSchema],
    batch: &ColumnBatch,
) -> Result<Value> {
    match expr {
        Expr::FunctionCall { name, args, .. } => {
            let upper = name.to_uppercase();
            match upper.as_str() {
                "COUNT" => {
                    // COUNT(*) or COUNT(col)
                    let is_star = args.is_empty()
                        || matches!(args.first(), Some(Expr::Star));
                    if is_star {
                        Ok(Value::Int64(batch.row_count as i64))
                    } else {
                        let col_idx = col_ref_idx(&args[0], schema, &batch.columns)
                            .ok_or_else(|| SqlError::Execution("COUNT: column not found".into()))?;
                        Ok(batch.aggregate_column(col_idx, "COUNT"))
                    }
                }
                "SUM" | "AVG" | "MIN" | "MAX" => {
                    if args.is_empty() {
                        return Ok(Value::Null);
                    }
                    let col_idx = col_ref_idx(&args[0], schema, &batch.columns)
                        .ok_or_else(|| SqlError::Execution(format!("{}: column not found", name)))?;
                    Ok(batch.aggregate_column(col_idx, &upper))
                }
                _ => Err(SqlError::Execution(format!(
                    "vectorized path: unsupported function '{}'",
                    name
                ))),
            }
        }
        Expr::ColumnRef { .. } => {
            // Passthrough: return the first row value in this (group) batch.
            let col_idx = col_ref_idx(expr, schema, &batch.columns)
                .ok_or_else(|| SqlError::Execution(format!("column not found: {:?}", expr)))?;
            Ok(batch.columns[col_idx].first().cloned().unwrap_or(Value::Null))
        }
        Expr::IntLit(n) => Ok(Value::Int64(*n)),
        Expr::FloatLit(f) => Ok(Value::Float64(*f)),
        Expr::StrLit(s) => Ok(Value::Text(s.clone())),
        Expr::BoolLit(b) => Ok(Value::Bool(*b)),
        Expr::Null => Ok(Value::Null),
        other => Err(SqlError::Execution(format!(
            "vectorized path: unsupported expression {:?}",
            other
        ))),
    }
}

// --- Helpers ---

fn col_ref_idx(expr: &Expr, schema: &[ColumnSchema], _columns: &[Vec<Value>]) -> Option<usize> {
    if let Expr::ColumnRef { column, .. } = expr {
        let col_lower = column.to_lowercase();
        schema.iter().position(|c| {
            let n = c.name.to_lowercase();
            n == col_lower || n.ends_with(&format!(".{}", col_lower))
        })
    } else {
        None
    }
}

fn cmp_op(op: &BinOp, lhs: &Value, rhs: &Value) -> bool {
    match op {
        BinOp::Eq => lhs == rhs,
        BinOp::NotEq => lhs != rhs,
        BinOp::Lt => lhs.partial_cmp(rhs).map(|o| o.is_lt()).unwrap_or(false),
        BinOp::Gt => lhs.partial_cmp(rhs).map(|o| o.is_gt()).unwrap_or(false),
        BinOp::LtEq => lhs.partial_cmp(rhs).map(|o| o.is_le()).unwrap_or(false),
        BinOp::GtEq => lhs.partial_cmp(rhs).map(|o| o.is_ge()).unwrap_or(false),
        _ => true,
    }
}

fn flip_op(op: &BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Gt => BinOp::Lt,
        BinOp::LtEq => BinOp::GtEq,
        BinOp::GtEq => BinOp::LtEq,
        other => other.clone(),
    }
}

fn columnar_to_sql(v: oigrap_storage::columnar::Value) -> Value {
    match v {
        oigrap_storage::columnar::Value::Null => Value::Null,
        oigrap_storage::columnar::Value::Bool(b) => Value::Bool(b),
        oigrap_storage::columnar::Value::Int64(n) => Value::Int64(n),
        oigrap_storage::columnar::Value::Float64(f) => Value::Float64(f),
        oigrap_storage::columnar::Value::Text(s) => Value::Text(s),
    }
}

/// Convert a sql Value into a columnar Value for zone-map comparisons.
fn sql_val_to_columnar(v: &Value) -> oigrap_storage::columnar::Value {
    match v {
        Value::Null => oigrap_storage::columnar::Value::Null,
        Value::Bool(b) => oigrap_storage::columnar::Value::Bool(*b),
        Value::Int64(n) => oigrap_storage::columnar::Value::Int64(*n),
        Value::Float64(f) => oigrap_storage::columnar::Value::Float64(*f),
        Value::Text(s) => oigrap_storage::columnar::Value::Text(s.clone()),
    }
}

/// Determine which zone indices (0-based) survive zone-map pruning for a given WHERE predicate.
/// Only simple `column op literal` predicates are handled; complex predicates return all zones.
/// Returns the list of zone indices that cannot be pruned.
fn prune_with_zone_maps(
    store: &ColumnarStore,
    schema: &[ColumnSchema],
    where_clause: &Expr,
) -> Vec<usize> {
    let n_zones = store.row_count.div_ceil(ZONE_SIZE);
    let all_zones: Vec<usize> = (0..n_zones).collect();

    // Only handle binary op predicates of form `col op literal`.
    let (col_name, op, bound_val) = match where_clause {
        Expr::BinaryOp { op, left, right } => {
            // col op literal
            if let (Some(name), Ok(val)) = (
                extract_col_name(left),
                eval_literal(right),
            ) {
                (name, op.clone(), val)
            }
            // literal op col (flip)
            else if let (Ok(val), Some(name)) = (
                eval_literal(left),
                extract_col_name(right),
            ) {
                (name, flip_op(op), val)
            } else {
                return all_zones;
            }
        }
        _ => return all_zones,
    };

    // Find column index in schema.
    let col_lower = col_name.to_lowercase();
    let col_idx = match schema.iter().position(|c| {
        let n = c.name.to_lowercase();
        n == col_lower || n.ends_with(&format!(".{}", col_lower))
    }) {
        Some(idx) => idx,
        None => return all_zones,
    };

    // No zone maps built yet or wrong col.
    if col_idx >= store.zone_maps.len() || store.zone_maps[col_idx].is_empty() {
        return all_zones;
    }

    let bound_col = sql_val_to_columnar(&bound_val);

    store.zone_maps[col_idx]
        .iter()
        .enumerate()
        .filter_map(|(zone_idx, zm)| {
            let pruned = match op {
                BinOp::Gt  => zm.can_prune_gt(&bound_col),
                BinOp::Lt  => zm.can_prune_lt(&bound_col),
                BinOp::GtEq => {
                    // col >= k: prune if zone.max < k  (max strictly less than k)
                    zm.max.partial_cmp(&bound_col)
                        .map(|o| o.is_lt())
                        .unwrap_or(false)
                }
                BinOp::LtEq => {
                    // col <= k: prune if zone.min > k
                    zm.min.partial_cmp(&bound_col)
                        .map(|o| o.is_gt())
                        .unwrap_or(false)
                }
                BinOp::Eq => {
                    // col = k: prune if k < zone.min or k > zone.max
                    let below_min = bound_col.partial_cmp(&zm.min)
                        .map(|o| o.is_lt())
                        .unwrap_or(false);
                    let above_max = bound_col.partial_cmp(&zm.max)
                        .map(|o| o.is_gt())
                        .unwrap_or(false);
                    below_min || above_max
                }
                _ => false,
            };
            if pruned { None } else { Some(zone_idx) }
        })
        .collect()
}

/// Build a ColumnBatch containing only the rows from the specified zones.
fn batch_from_zones(store: &ColumnarStore, surviving_zones: &[usize]) -> ColumnBatch {
    let col_count = store.columns.len();
    if col_count == 0 || surviving_zones.is_empty() {
        return ColumnBatch { columns: vec![Vec::new(); col_count.max(1)], row_count: 0 };
    }

    // Materialize all columns once.
    let all_cols: Vec<Vec<oigrap_storage::columnar::Value>> = (0..col_count)
        .map(|i| store.scan_column(i))
        .collect();

    let mut columns: Vec<Vec<Value>> = vec![Vec::new(); col_count];
    let mut row_count = 0usize;

    for &zone_idx in surviving_zones {
        let start = zone_idx * ZONE_SIZE;
        let end = ((zone_idx + 1) * ZONE_SIZE).min(store.row_count);
        if start >= store.row_count {
            continue;
        }
        let zone_rows = end - start;
        row_count += zone_rows;
        for ci in 0..col_count {
            for ri in start..end {
                let v = if ri < all_cols[ci].len() {
                    all_cols[ci][ri].clone()
                } else {
                    oigrap_storage::columnar::Value::Null
                };
                columns[ci].push(columnar_to_sql(v));
            }
        }
    }

    ColumnBatch { columns, row_count }
}

/// Helper: extract the column name string from a ColumnRef expression.
fn extract_col_name(expr: &Expr) -> Option<String> {
    if let Expr::ColumnRef { column, .. } = expr {
        Some(column.clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oigrap_storage::ColumnarStore;

    fn make_int_store(values: &[i64]) -> ColumnarStore {
        let mut store = ColumnarStore::new();
        let col_names = vec!["n".to_string()];
        let rows: Vec<Vec<oigrap_storage::columnar::Value>> = values
            .iter()
            .map(|&n| vec![oigrap_storage::columnar::Value::Int64(n)])
            .collect();
        store.insert_rows(&col_names, &rows);
        store
    }

    fn simple_schema() -> Vec<ColumnSchema> {
        vec![ColumnSchema {
            name: "n".to_string(),
            sql_type: crate::catalog::SqlType::Int64,
            nullable: false,
            primary_key: false,
        }]
    }

    #[test]
    fn test_vectorized_count() {
        let store = make_int_store(&(0i64..100).collect::<Vec<_>>());
        let schema = simple_schema();
        let batch = ColumnBatch::from_columnar(&store);
        assert_eq!(batch.row_count, 100);
        let result = batch.aggregate_column(0, "COUNT");
        assert_eq!(result, Value::Int64(100));
    }

    #[test]
    fn test_vectorized_filter() {
        let store = make_int_store(&(0i64..100).collect::<Vec<_>>());
        let schema = simple_schema();
        let batch = ColumnBatch::from_columnar(&store);

        // Filter n > 50 => rows 51..99 => 49 rows
        let filter_expr = Expr::BinaryOp {
            op: BinOp::Gt,
            left: Box::new(Expr::ColumnRef { table: None, column: "n".to_string() }),
            right: Box::new(Expr::IntLit(50)),
        };
        let mask = batch.eval_filter(&filter_expr, &schema);
        let filtered = batch.filter(&mask);
        assert_eq!(filtered.row_count, 49, "expected 49 rows with n > 50");
        let count = filtered.aggregate_column(0, "COUNT");
        assert_eq!(count, Value::Int64(49));
    }

    #[test]
    fn test_vectorized_sum() {
        // Sum of 1..=100 = 5050
        let store = make_int_store(&(1i64..=100).collect::<Vec<_>>());
        let schema = simple_schema();
        let batch = ColumnBatch::from_columnar(&store);
        let result = batch.aggregate_column(0, "SUM");
        assert_eq!(result, Value::Int64(5050));
    }

    #[test]
    fn test_vectorized_aggregate_fn() {
        let store = make_int_store(&(0i64..100).collect::<Vec<_>>());
        let schema = simple_schema();
        let select_cols = vec![SelectColumn::Expr {
            expr: Expr::FunctionCall {
                name: "COUNT".to_string(),
                args: vec![Expr::Star],
                distinct: false,
            },
            alias: Some("cnt".to_string()),
        }];
        let (col_names, rows) =
            vectorized_aggregate(&store, &schema, &select_cols, &None, &[]).unwrap();
        assert_eq!(col_names, vec!["cnt"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int64(100));
    }

    #[test]
    fn test_vectorized_zone_map_pruning() {
        use crate::catalog::SqlType;
        use oigrap_storage::columnar::ZONE_SIZE;

        // Build a store with 3000 rows: column "x" = 0..2999, column "y" = 1 for all rows.
        let mut store = ColumnarStore::new();
        let col_names = vec!["x".to_string(), "y".to_string()];
        let mut rows: Vec<Vec<oigrap_storage::columnar::Value>> = Vec::new();
        for i in 0..3000i64 {
            rows.push(vec![
                oigrap_storage::columnar::Value::Int64(i),
                oigrap_storage::columnar::Value::Int64(1),
            ]);
        }
        store.insert_rows(&col_names, &rows);
        store.build_zone_maps();

        let schema = vec![
            ColumnSchema {
                name: "x".to_string(),
                sql_type: SqlType::Int64,
                nullable: false,
                primary_key: false,
            },
            ColumnSchema {
                name: "y".to_string(),
                sql_type: SqlType::Int64,
                nullable: false,
                primary_key: false,
            },
        ];

        // WHERE x > 2500 — zone 0 (max=999) and zone 1 (max=1999) should be pruned.
        // Only zone 2 (max=2999 > 2500) survives.
        let where_expr = Expr::BinaryOp {
            op: BinOp::Gt,
            left: Box::new(Expr::ColumnRef { table: None, column: "x".to_string() }),
            right: Box::new(Expr::IntLit(2500)),
        };

        // Verify zone pruning directly.
        let surviving = prune_with_zone_maps(&store, &schema, &where_expr);
        assert_eq!(surviving.len(), 1, "only zone 2 should survive");
        assert_eq!(surviving[0], 2, "surviving zone should be index 2");

        // SELECT SUM(y) WHERE x > 2500
        // Rows with x in 2501..2999 => 499 rows each with y=1 => SUM(y) = 499.
        let select_cols = vec![SelectColumn::Expr {
            expr: Expr::FunctionCall {
                name: "SUM".to_string(),
                args: vec![Expr::ColumnRef { table: None, column: "y".to_string() }],
                distinct: false,
            },
            alias: Some("total".to_string()),
        }];

        let (col_names_out, result_rows) = vectorized_aggregate(
            &store,
            &schema,
            &select_cols,
            &Some(where_expr),
            &[],
        )
        .unwrap();

        assert_eq!(col_names_out, vec!["total"]);
        assert_eq!(result_rows.len(), 1);
        // Rows 2501..=2999 is 499 rows.
        assert_eq!(result_rows[0][0], Value::Int64(499));

        // Also verify ZONE_SIZE is 1000 as expected.
        assert_eq!(ZONE_SIZE, 1000);
    }

    #[test]
    fn test_vectorized_group_by() {
        use crate::catalog::SqlType;

        // Build a store with columns: category (Text), amount (Int64)
        let mut store = ColumnarStore::new();
        let col_names = vec!["category".to_string(), "amount".to_string()];
        let mut rows: Vec<Vec<oigrap_storage::columnar::Value>> = Vec::new();
        // 3 rows for "A": 10, 20, 30 -> sum=60
        for &amt in &[10i64, 20, 30] {
            rows.push(vec![
                oigrap_storage::columnar::Value::Text("A".to_string()),
                oigrap_storage::columnar::Value::Int64(amt),
            ]);
        }
        // 3 rows for "B": 5, 15, 25 -> sum=45
        for &amt in &[5i64, 15, 25] {
            rows.push(vec![
                oigrap_storage::columnar::Value::Text("B".to_string()),
                oigrap_storage::columnar::Value::Int64(amt),
            ]);
        }
        // 3 rows for "C": 100, 200, 300 -> sum=600
        for &amt in &[100i64, 200, 300] {
            rows.push(vec![
                oigrap_storage::columnar::Value::Text("C".to_string()),
                oigrap_storage::columnar::Value::Int64(amt),
            ]);
        }
        store.insert_rows(&col_names, &rows);

        let schema = vec![
            ColumnSchema {
                name: "category".to_string(),
                sql_type: SqlType::Text,
                nullable: false,
                primary_key: false,
            },
            ColumnSchema {
                name: "amount".to_string(),
                sql_type: SqlType::Int64,
                nullable: false,
                primary_key: false,
            },
        ];

        let select_cols = vec![
            SelectColumn::Expr {
                expr: Expr::ColumnRef { table: None, column: "category".to_string() },
                alias: Some("category".to_string()),
            },
            SelectColumn::Expr {
                expr: Expr::FunctionCall {
                    name: "SUM".to_string(),
                    args: vec![Expr::ColumnRef { table: None, column: "amount".to_string() }],
                    distinct: false,
                },
                alias: Some("total".to_string()),
            },
        ];

        let group_by = vec![Expr::ColumnRef { table: None, column: "category".to_string() }];

        let (col_names, result_rows) =
            vectorized_aggregate(&store, &schema, &select_cols, &None, &group_by).unwrap();

        assert_eq!(col_names, vec!["category", "total"]);
        assert_eq!(result_rows.len(), 3, "expected 3 groups");

        // Build a map from category to sum for flexible ordering.
        use std::collections::HashMap;
        let mut sums: HashMap<String, i64> = HashMap::new();
        for row in &result_rows {
            let cat = match &row[0] {
                Value::Text(s) => s.clone(),
                other => panic!("expected Text for category, got {:?}", other),
            };
            let sum = match row[1] {
                Value::Int64(n) => n,
                ref other => panic!("expected Int64 for sum, got {:?}", other),
            };
            sums.insert(cat, sum);
        }

        assert_eq!(sums.get("A").copied(), Some(60), "A sum should be 60");
        assert_eq!(sums.get("B").copied(), Some(45), "B sum should be 45");
        assert_eq!(sums.get("C").copied(), Some(600), "C sum should be 600");
    }
}
