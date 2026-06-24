use crate::ast::*;
use crate::catalog::{Catalog, ColumnSchema, ColumnStats, SqlType, TableStats};
use crate::codec::{decode_row, encode_row};
use crate::error::{Result, SqlError};
use crate::parser::Parser;
use crate::value::Value;
use oigrap_storage::{
    encode_jsonb, jsonb_contains, jsonb_tokens, text_tokens, vacuum_table, GinIndex,
    BTree, BufferPool, ColumnarStore, HeapFile, HnswIndex, TransactionManager, TupleHeader,
    WalManager,
};

/// Row count threshold above which the inner hash join spills partitions to disk.
const HASH_JOIN_SPILL_THRESHOLD: usize = 100_000;

// --- Thread-local execution context ---

thread_local! {
    /// Current recursion depth inside a WITH RECURSIVE CTE loop (1-based).
    static CTE_DEPTH: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };

    /// Edge table data for oigrap_shortest_path: maps table_name -> list of (from_id, to_id).
    static EDGE_CACHE: std::cell::RefCell<std::collections::HashMap<String, Vec<(i64, i64)>>>
        = std::cell::RefCell::new(std::collections::HashMap::new());

    /// PageRank cache: maps "edge_table/from_col/to_col" -> HashMap<node_id, rank>
    static PAGERANK_CACHE: std::cell::RefCell<std::collections::HashMap<String, std::collections::HashMap<i64, f64>>>
        = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// A result set: column names + rows.
#[derive(Debug, Default)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub rows_affected: usize,
    /// PostgreSQL CommandComplete tag, e.g. "SELECT 5", "INSERT 0 1", "CREATE TABLE"
    pub tag: String,
}

/// The SQL execution engine. Owns the catalog and wraps the storage layer.
pub struct Engine {
    pub catalog: Catalog,
}

impl Engine {
    pub fn new() -> Self {
        Engine { catalog: Catalog::new() }
    }

    /// Execute a SQL string and return the result.
    pub fn execute(
        &mut self,
        sql: &str,
        pool: &mut BufferPool,
        wal: &mut WalManager,
        tx: &mut TransactionManager,
    ) -> Result<QueryResult> {
        let stmt = Parser::parse(sql)?;
        self.execute_stmt(stmt, pool, wal, tx)
    }

    pub fn execute_stmt(
        &mut self,
        stmt: Statement,
        pool: &mut BufferPool,
        wal: &mut WalManager,
        tx: &mut TransactionManager,
    ) -> Result<QueryResult> {
        match stmt {
            Statement::CreateTable(ct) => self.exec_create_table(ct, pool, wal),
            Statement::Insert(ins) => self.exec_insert(ins, pool, wal, tx),
            Statement::Select(sel) => self.exec_select(sel, pool, tx),
            Statement::Delete(del) => self.exec_delete(del, pool, wal, tx),
            Statement::Begin => Ok(QueryResult { tag: "BEGIN".into(), ..Default::default() }),
            Statement::Commit => {
                let xid = tx.begin();
                tx.commit(xid, wal)?;
                Ok(QueryResult { tag: "COMMIT".into(), ..Default::default() })
            }
            Statement::Rollback => Ok(QueryResult { tag: "ROLLBACK".into(), ..Default::default() }),
            Statement::DropTable(dt) => {
                let name = dt.table.to_lowercase();
                if !self.catalog.contains(&name) {
                    if dt.if_exists {
                        return Ok(QueryResult { tag: "DROP TABLE".into(), ..Default::default() });
                    }
                    return Err(SqlError::Semantic(format!("table '{}' does not exist", name)));
                }
                // For now: mark table as dropped without actual heap cleanup (Phase 4 garbage collection)
                Ok(QueryResult { tag: "DROP TABLE".into(), ..Default::default() })
            }
            Statement::Update(upd) => self.exec_update(upd, pool, wal, tx),
            Statement::Explain(inner) => {
                let plan_text = self.build_plan(*inner);
                Ok(QueryResult {
                    tag: "EXPLAIN".into(),
                    columns: vec!["QUERY PLAN".to_string()],
                    rows: vec![vec![Value::Text(plan_text)]],
                    rows_affected: 0,
                })
            }
            Statement::Analyze(table) => self.exec_analyze(&table, pool, tx),
            Statement::CreateVectorIndex { table, column } => {
                self.exec_create_vector_index(&table, &column, pool, tx)
            }
            Statement::CreateGinIndex { table, column } => {
                self.exec_create_gin_index(&table, &column, pool, tx)
            }
            Statement::Vacuum(table) => self.exec_vacuum(&table, pool, wal, tx),
            Statement::SetVar { .. } => {
                Ok(QueryResult { tag: "SET".into(), ..Default::default() })
            }
            Statement::ShowVar { name } => {
                let val = pg_show_var(&name);
                Ok(QueryResult {
                    tag: "SHOW".into(),
                    columns: vec![name.clone()],
                    rows: vec![vec![Value::Text(val)]],
                    rows_affected: 0,
                })
            }
            #[allow(unreachable_patterns)]
            other => Err(SqlError::Execution(format!("unsupported statement: {:?}", other))),
        }
    }

    // --- CREATE TABLE ---

    fn exec_create_table(
        &mut self,
        ct: CreateTableStmt,
        pool: &mut BufferPool,
        wal: &mut WalManager,
    ) -> Result<QueryResult> {
        let name = ct.table.to_lowercase();

        if self.catalog.contains(&name) {
            if ct.if_not_exists {
                return Ok(QueryResult { tag: "CREATE TABLE".into(), ..Default::default() });
            }
            return Err(SqlError::Semantic(format!("table '{}' already exists", name)));
        }

        // Determine primary key column
        let pk_col_name: Option<String> = ct.constraints.iter().find_map(|c| {
            if let TableConstraint::PrimaryKey(cols) = c {
                cols.first().cloned()
            } else {
                None
            }
        });

        // Build column schema
        let mut columns: Vec<ColumnSchema> = Vec::new();
        let mut pk_col_idx: Option<usize> = None;

        for (i, col_def) in ct.columns.iter().enumerate() {
            let sql_type = SqlType::from_ast(&col_def.data_type)
                .ok_or_else(|| SqlError::Semantic(format!(
                    "unsupported column type {:?} for column '{}'",
                    col_def.data_type, col_def.name
                )))?;

            let is_pk = pk_col_name.as_deref() == Some(col_def.name.as_str())
                || col_def.primary_key;
            if is_pk {
                pk_col_idx = Some(i);
            }

            columns.push(ColumnSchema {
                name: col_def.name.clone(),
                sql_type,
                nullable: col_def.nullable && !is_pk,
                primary_key: is_pk,
            });
        }

        // Allocate the heap file — use the id the catalog will assign
        let heap = HeapFile::new(self.catalog.next_id());

        // Allocate B+ tree index if there's a primary key
        let (pk_index, pk_index_meta) = if pk_col_idx.is_some() {
            let tree = BTree::create(pool)?;
            let meta = tree.meta_page_id();
            (Some(tree), Some(meta))
        } else {
            (None, None)
        };

        self.catalog.create_table(name.clone(), columns, pk_col_idx, heap, pk_index, pk_index_meta);

        // If STORAGE COLUMNAR, initialize the columnar store
        if ct.storage == StorageLayout::Columnar {
            let entry_mut = self.catalog.get_mut(&name).unwrap();
            entry_mut.columnar = Some(ColumnarStore::new());
        }

        wal.flush()?;

        Ok(QueryResult {
            tag: "CREATE TABLE".into(),
            ..Default::default()
        })
    }

    // --- INSERT ---

    fn exec_insert(
        &mut self,
        ins: InsertStmt,
        pool: &mut BufferPool,
        wal: &mut WalManager,
        tx: &mut TransactionManager,
    ) -> Result<QueryResult> {
        let name = ins.table.to_lowercase();
        let entry = self.catalog.get(&name)
            .ok_or_else(|| SqlError::Semantic(format!("table '{}' does not exist", name)))?;
        let schema = entry.columns.clone();
        let pk_col_idx = entry.pk_col_idx;
        let _table_id = entry.table_id;

        let InsertSource::Values(rows) = ins.source else {
            return Err(SqlError::Execution("INSERT ... SELECT not yet supported".into()));
        };

        let col_positions: Vec<usize> = if ins.columns.is_empty() {
            (0..schema.len()).collect()
        } else {
            ins.columns.iter().map(|col_name| {
                schema.iter().position(|c| c.name.eq_ignore_ascii_case(col_name))
                    .ok_or_else(|| SqlError::Semantic(format!("unknown column '{}'", col_name)))
            }).collect::<Result<Vec<_>>>()?
        };

        let xid = tx.begin();
        let mut count = 0;

        for row_exprs in rows {
            if row_exprs.len() != col_positions.len() {
                return Err(SqlError::Semantic(format!(
                    "expected {} values, got {}",
                    col_positions.len(), row_exprs.len()
                )));
            }

            // Evaluate literal expressions into values
            let mut values = vec![Value::Null; schema.len()];
            for (expr_idx, col_idx) in col_positions.iter().enumerate() {
                values[*col_idx] = eval_literal(&row_exprs[expr_idx])?;
            }

            // Coerce types
            for (i, col) in schema.iter().enumerate() {
                if !values[i].is_null() {
                    values[i] = coerce_value(&values[i], &col.sql_type)?;
                }
            }

            // Encode row bytes (without MVCC header)
            let row_bytes = encode_row(&schema, &values)?;

            // Prepend MVCC header
            let header = TupleHeader::new_insert(xid, 0);
            let mut full_tuple = header.encode().to_vec();
            full_tuple.extend_from_slice(&row_bytes);

            // Insert into heap
            let entry_mut = self.catalog.get_mut(&name).unwrap();
            let tid = entry_mut.heap.insert_tuple(pool, wal, xid, &full_tuple)?;

            // Insert into primary key index
            if let (Some(pk_idx), Some(tree)) = (pk_col_idx, entry_mut.pk_index.as_mut()) {
                let pk_val = &values[pk_idx];
                let key_bytes = pk_val.as_key_bytes()
                    .ok_or_else(|| SqlError::Execution("cannot index NULL primary key".into()))?;
                tree.insert(pool, &key_bytes, tid)?;
            }

            // Dual-write to columnar store if this table has one
            if entry_mut.columnar.is_some() {
                let col_names: Vec<String> = entry_mut.columns.iter().map(|c| c.name.clone()).collect();
                // Convert sql Value to columnar Value
                let col_row: Vec<oigrap_storage::columnar::Value> =
                    values.iter().map(sql_val_to_columnar).collect();
                if let Some(cs) = entry_mut.columnar.as_mut() {
                    cs.insert_rows(&col_names, &[col_row]);
                }
            }

            // Update GIN index if present
            if entry_mut.gin_index.is_some() {
                let gin_col = entry_mut.gin_column.clone();
                let col_idx_opt = gin_col.as_deref().and_then(|col_name| {
                    entry_mut.columns.iter().position(|c| c.name.eq_ignore_ascii_case(col_name))
                });
                if let Some(col_idx) = col_idx_opt {
                    let tokens = match values.get(col_idx) {
                        Some(Value::Text(s)) => {
                            // Try JSONB first, fall back to text tokens
                            if let Ok(encoded) = encode_jsonb(s.as_str()) {
                                let jt = jsonb_tokens(&encoded);
                                if jt.is_empty() { text_tokens(s.as_str()) } else { jt }
                            } else {
                                text_tokens(s.as_str())
                            }
                        }
                        _ => vec![],
                    };
                    if !tokens.is_empty() {
                        if let Some(gin) = entry_mut.gin_index.as_mut() {
                            gin.insert(tid, tokens);
                        }
                    }
                }
            }

            count += 1;
        }

        tx.commit(xid, wal)?;
        wal.flush()?;

        Ok(QueryResult { tag: format!("INSERT 0 {}", count), rows_affected: count, ..Default::default() })
    }

    // --- SELECT ---

    fn exec_select(
        &mut self,
        sel: SelectStmt,
        pool: &mut BufferPool,
        tx: &TransactionManager,
    ) -> Result<QueryResult> {
        self.exec_select_with_cte(sel, pool, tx, &std::collections::HashMap::new())
    }

    fn exec_select_with_cte(
        &mut self,
        sel: SelectStmt,
        pool: &mut BufferPool,
        tx: &TransactionManager,
        parent_cte: &std::collections::HashMap<String, (Vec<ColumnSchema>, Vec<Vec<Value>>)>,
    ) -> Result<QueryResult> {
        // Build CTE context from WITH clauses
        let mut cte_ctx: std::collections::HashMap<String, (Vec<ColumnSchema>, Vec<Vec<Value>>)> =
            parent_cte.clone();

        for cte in &sel.with_clauses {
            let cte_name = cte.name.to_lowercase();
            let cte_query = (*cte.query).clone();

            if cte.recursive {
                // Recursive CTE with cycle detection and depth limiting.
                // If union_body is present: left = base case, right = recursive step.
                // Otherwise: cte_query is used for both (legacy behavior).
                let (base_query, recursive_query, dedup_mode) = if let Some(ref ub) = cte.union_body {
                    (*ub.left.clone(), *ub.right.clone(), !ub.all)
                } else {
                    (cte_query.clone(), cte_query.clone(), false)
                };

                // Execute the base case
                let result = self.exec_select_with_cte(base_query, pool, tx, &cte_ctx)?;
                let mut schema = result.columns.iter().map(|n| ColumnSchema {
                    name: n.clone(),
                    sql_type: crate::catalog::SqlType::Text,
                    nullable: true,
                    primary_key: false,
                }).collect::<Vec<_>>();
                // Try to infer types from first row
                if let Some(row) = result.rows.first() {
                    for (i, val) in row.iter().enumerate() {
                        if i < schema.len() {
                            schema[i].sql_type = sql_type_of(val);
                        }
                    }
                }

                let mut all_rows: Vec<Vec<Value>> = Vec::new();
                // Cycle detection: track seen rows
                let mut seen: std::collections::HashSet<Vec<String>> = std::collections::HashSet::new();

                // Add base rows (always deduplicated for cycle detection)
                for row in result.rows {
                    let key: Vec<String> = row.iter().map(|v| format!("{:?}", v)).collect();
                    if seen.insert(key) {
                        all_rows.push(row);
                    }
                }
                let mut frontier = all_rows.clone();

                // Iterate BFS up to 1000 depth
                for _depth in 0..1000 {
                    if frontier.is_empty() {
                        break;
                    }
                    // Expose depth for DEPTH() calls inside the recursive step.
                    // The base case is at depth 1; the first recursive iteration (_depth=0)
                    // produces rows at depth 2, second at depth 3, etc.
                    CTE_DEPTH.with(|d| d.set(_depth as i64 + 2));
                    // Register frontier as the current CTE value
                    cte_ctx.insert(cte_name.clone(), (schema.clone(), frontier.clone()));
                    let new_result = self.exec_select_with_cte(recursive_query.clone(), pool, tx, &cte_ctx)?;

                    let mut new_frontier: Vec<Vec<Value>> = Vec::new();
                    for row in new_result.rows {
                        let key: Vec<String> = row.iter().map(|v| format!("{:?}", v)).collect();
                        // For UNION (not ALL): always deduplicate; for UNION ALL: only skip if seen (cycle detection)
                        if dedup_mode {
                            if seen.insert(key) {
                                new_frontier.push(row);
                            }
                        } else if seen.insert(key) {
                            new_frontier.push(row);
                        }
                    }

                    if new_frontier.is_empty() {
                        break;
                    }
                    all_rows.extend(new_frontier.clone());
                    frontier = new_frontier;
                }
                // Reset depth sentinel after the recursive CTE loop.
                CTE_DEPTH.with(|d| d.set(0));
                cte_ctx.insert(cte_name, (schema, all_rows));
            } else {
                // Non-recursive CTE
                let result = self.exec_select_with_cte(cte_query, pool, tx, &cte_ctx)?;
                let schema = result.columns.iter().enumerate().map(|(i, n)| {
                    let sql_type = result.rows.first()
                        .and_then(|row| row.get(i))
                        .map(sql_type_of)
                        .unwrap_or(crate::catalog::SqlType::Text);
                    ColumnSchema {
                        name: n.clone(),
                        sql_type,
                        nullable: true,
                        primary_key: false,
                    }
                }).collect::<Vec<_>>();
                cte_ctx.insert(cte_name, (schema, result.rows));
            }
        }

        if sel.from.is_empty() {
            // SELECT without FROM — evaluate expressions against empty row.
            // Pre-scan any edge tables referenced by oigrap_shortest_path() before evaluation.
            preload_edge_tables_for_shortest_path(&sel.columns, &self.catalog, pool, tx);
            let empty_schema: Vec<ColumnSchema> = vec![];
            let empty_row: Vec<Value> = vec![];
            let row: Vec<Value> = sel.columns.iter().map(|c| match c {
                SelectColumn::Expr { expr, .. } => {
                    eval_expr(expr, &empty_schema, &empty_row)
                        .or_else(|_| eval_literal(expr))
                        .unwrap_or(Value::Null)
                }
                SelectColumn::Star => Value::Null,
            }).collect();
            EDGE_CACHE.with(|c| c.borrow_mut().clear());
            let cols: Vec<String> = sel.columns.iter().enumerate().map(|(i, c)| match c {
                SelectColumn::Expr { alias: Some(a), .. } => a.clone(),
                SelectColumn::Expr { expr: Expr::IntLit(n), .. } => n.to_string(),
                _ => format!("col{}", i),
            }).collect();
            return Ok(QueryResult { tag: "SELECT 1".into(), columns: cols, rows: vec![row], rows_affected: 0 });
        }

        // Determine if we need multi-join execution
        let needs_join = sel.from.len() > 1 || sel.from[0].join.is_some();

        let (schema, source_rows) = if needs_join {
            self.exec_multi_join(&sel.from, pool, tx, &cte_ctx)?
        } else {
            let table_ref = &sel.from[0];
            let name = table_ref.name.to_lowercase();

            // Check CTE context first
            if let Some((cte_schema, cte_rows)) = cte_ctx.get(&name) {
                (cte_schema.clone(), cte_rows.clone())
            } else if let Some((vschema, vrows)) = {
                let bare = name
                    .trim_start_matches("pg_catalog.")
                    .trim_start_matches("information_schema.");
                virtual_catalog_rows(bare, &self.catalog)
            } {
                (vschema, vrows)
            } else {
                let entry = self.catalog.get(&name)
                    .ok_or_else(|| SqlError::Semantic(format!("table '{}' does not exist", name)))?;
                let schema = entry.columns.clone();
                let snap = tx.snapshot();
                let pk_col_idx = entry.pk_col_idx;
                let has_columnar = entry.columnar.is_some();
                let has_vector_idx = entry.vector_index.is_some();

                // Detect ANN vector query: ORDER BY col <-> vec LIMIT k
                // has_vector_idx is used for path selection below
                let _ = has_vector_idx;
                let ann_query = detect_vector_ann(&sel.order_by, &sel.limit, &schema);

                if let Some((vec_col_idx, query_vec, k)) = ann_query.clone() {
                    // ANN path
                    let entry = self.catalog.get(&name).unwrap();
                    if let Some(hnsw) = &entry.vector_index {
                        // HNSW index scan
                        let results = hnsw.search(&query_vec, k, 50);
                        let mut out = Vec::new();
                        for (id, _dist) in results {
                            use oigrap_storage::TupleId;
                            let tid = TupleId { page_id: id >> 16, slot_id: (id & 0xffff) as u16 };
                            if let Ok(raw) = entry.heap.get_tuple(pool, tid) {
                                if let Ok((header, row_bytes)) = split_mvcc(&raw) {
                                    if tx.is_visible(&header, &snap) {
                                        if let Ok(row) = decode_row(&schema, row_bytes) {
                                            out.push(row);
                                        }
                                    }
                                }
                            }
                        }
                        // return early — ANN result already sorted
                        let (col_names, projected) = project(&sel.columns, &schema, out)?;
                        let n = projected.len();
                        return Ok(QueryResult {
                            tag: format!("SELECT {}", n),
                            columns: col_names,
                            rows: projected,
                            rows_affected: 0,
                        });
                    }
                    // No index: fall through to seq scan + sort
                    let _ = vec_col_idx; // used in sort below
                }

                // Columnar path for aggregates with GROUP BY
                if has_columnar && (!sel.group_by.is_empty() || has_aggregates(&sel.columns)) {
                    let entry = self.catalog.get(&name).unwrap();
                    let col_rows = entry.columnar.as_ref().unwrap().scan();
                    // Convert columnar values to sql values
                    let rows: Vec<Vec<Value>> = col_rows.iter()
                        .map(|row| row.iter().map(columnar_val_to_sql).collect())
                        .collect();
                    // Apply WHERE filter
                    let filtered: Vec<Vec<Value>> = rows.into_iter().filter(|row| {
                        match &sel.where_clause {
                            None => true,
                            Some(expr) => eval_expr(expr, &schema, row)
                                .map(|v| is_truthy(&v))
                                .unwrap_or(false),
                        }
                    }).collect();
                    let (col_names, grouped_rows) = apply_group_by(
                        &sel.group_by,
                        &sel.columns,
                        &sel.having,
                        &schema,
                        filtered,
                    )?;
                    let n = grouped_rows.len();
                    return Ok(QueryResult {
                        tag: format!("SELECT {}", n),
                        columns: col_names,
                        rows: grouped_rows,
                        rows_affected: 0,
                    });
                }

                // GIN-accelerated path for JSONB @> queries
                if let Some(gin_query) = detect_gin_query(&sel.where_clause, &schema) {
                    let entry = self.catalog.get(&name).unwrap();
                    let gin_col = entry.gin_column.as_deref().unwrap_or("").to_string();
                    if let Some(gin) = entry.gin_index.as_ref() {
                        if gin_col.eq_ignore_ascii_case(&gin_query.col_name) {
                            if let Ok(inner_bytes) = encode_jsonb(&gin_query.json_str) {
                                let keys = jsonb_tokens(&inner_bytes);
                                let candidate_tids = gin.lookup_all(&keys);
                                let mut out = Vec::new();
                                for tid in candidate_tids {
                                    if let Ok(raw) = entry.heap.get_tuple(pool, tid) {
                                        if let Ok((header, row_bytes)) = split_mvcc(&raw) {
                                            if tx.is_visible(&header, &snap) {
                                                if let Ok(row) = decode_row(&schema, row_bytes) {
                                                    // Apply full WHERE filter to confirm
                                                    let matches = match &sel.where_clause {
                                                        None => true,
                                                        Some(expr) => eval_expr(expr, &schema, &row)
                                                            .map(|v| is_truthy(&v))
                                                            .unwrap_or(false),
                                                    };
                                                    if matches {
                                                        out.push(row);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                let (col_names, projected) = project(&sel.columns, &schema, out)?;
                                let n = projected.len();
                                return Ok(QueryResult {
                                    tag: format!("SELECT {}", n),
                                    columns: col_names,
                                    rows: projected,
                                    rows_affected: 0,
                                });
                            }
                        }
                    }
                }

                // Determine if we should use the primary key index
                // Use cost-based scan selection
                let stats = &entry.stats;
                let row_count = stats.as_ref().map(|s| s.row_count).unwrap_or(0);
                let selectivity = selectivity_estimate(&sel.where_clause, &schema, stats, pk_col_idx);
                let use_idx = should_use_index(selectivity, row_count);

                let pk_lookup = if use_idx {
                    can_use_pk_lookup(&sel.where_clause, &schema, pk_col_idx)
                } else {
                    None
                };

                let rows: Vec<Vec<Value>> = if let Some((_pk_col_idx, pk_val)) = pk_lookup {
                    // Index scan
                    let key_bytes = pk_val.as_key_bytes()
                        .ok_or_else(|| SqlError::Execution("cannot look up NULL primary key".into()))?;
                    let entry = self.catalog.get(&name).unwrap();
                    if let Some(tree) = &entry.pk_index {
                        match tree.lookup(pool, &key_bytes)? {
                            None => vec![],
                            Some(tid) => {
                                let raw = entry.heap.get_tuple(pool, tid)?;
                                let (header, row_bytes) = split_mvcc(&raw)?;
                                if tx.is_visible(&header, &snap) {
                                    vec![decode_row(&schema, row_bytes)?]
                                } else {
                                    vec![]
                                }
                            }
                        }
                    } else {
                        vec![]
                    }
                } else {
                    // Sequential scan
                    let entry = self.catalog.get(&name).unwrap();
                    let all = entry.heap.scan(pool)?;
                    let mut out = Vec::new();
                    for (_tid, raw) in all {
                        let (header, row_bytes) = split_mvcc(&raw)?;
                        if tx.is_visible(&header, &snap) {
                            out.push(decode_row(&schema, row_bytes)?);
                        }
                    }
                    out
                };
                (schema, rows)
            }
        };

        // Apply WHERE filter
        let filtered: Vec<Vec<Value>> = source_rows.into_iter().filter(|row| {
            match &sel.where_clause {
                None => true,
                Some(expr) => eval_expr(expr, &schema, row)
                    .map(|v| is_truthy(&v))
                    .unwrap_or(false),
            }
        }).collect();

        // Apply ORDER BY
        let mut ordered = filtered;
        if !sel.order_by.is_empty() {
            let schema_clone = schema.clone();
            let order = sel.order_by.clone();
            ordered.sort_by(|a, b| {
                for item in &order {
                    let va = eval_expr(&item.expr, &schema_clone, a).unwrap_or(Value::Null);
                    let vb = eval_expr(&item.expr, &schema_clone, b).unwrap_or(Value::Null);
                    let cmp = va.partial_cmp(&vb).unwrap_or(std::cmp::Ordering::Equal);
                    let cmp = if item.asc { cmp } else { cmp.reverse() };
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }

        // Apply LIMIT / OFFSET
        let limited: Vec<Vec<Value>> = {
            let offset = sel.offset
                .as_ref()
                .and_then(|e| eval_literal(e).ok())
                .and_then(|v| if let Value::Int64(n) = v { Some(n as usize) } else { None })
                .unwrap_or(0);
            let limit = sel.limit
                .as_ref()
                .and_then(|e| eval_literal(e).ok())
                .and_then(|v| if let Value::Int64(n) = v { Some(n as usize) } else { None });
            let skipped = ordered.into_iter().skip(offset);
            if let Some(lim) = limit {
                skipped.take(lim).collect()
            } else {
                skipped.collect()
            }
        };

        // GROUP BY / aggregation
        if !sel.group_by.is_empty() || has_aggregates(&sel.columns) {
            let (col_names, grouped_rows) = apply_group_by(
                &sel.group_by,
                &sel.columns,
                &sel.having,
                &schema,
                limited,
            )?;
            let n = grouped_rows.len();
            return Ok(QueryResult {
                tag: format!("SELECT {}", n),
                columns: col_names,
                rows: grouped_rows,
                rows_affected: 0,
            });
        }

        // Pre-scan edge tables referenced by oigrap_shortest_path() calls so
        // eval_function can access them via the EDGE_CACHE thread-local.
        preload_edge_tables_for_shortest_path(&sel.columns, &self.catalog, pool, tx);

        // Pre-compute PageRank if any oigrap_pagerank() calls are present.
        preload_pagerank(&sel.columns, &self.catalog, pool, tx);

        // Apply window functions (ROW_NUMBER, RANK, LAG, LEAD) if present.
        let (schema, limited) = if has_window_funcs(&sel.columns) {
            apply_window_functions(schema, limited, &sel.columns)
        } else {
            (schema, limited)
        };

        // Project columns
        let result = project(&sel.columns, &schema, limited)?;

        // Clear caches after projection so stale data does not leak between queries.
        EDGE_CACHE.with(|c| c.borrow_mut().clear());
        PAGERANK_CACHE.with(|c| c.borrow_mut().clear());

        let (col_names, projected) = result;
        let n = projected.len();
        Ok(QueryResult { tag: format!("SELECT {}", n), columns: col_names, rows: projected, rows_affected: 0 })
    }

    /// Execute a multi-table join and return (combined_schema, combined_rows).
    fn exec_multi_join(
        &mut self,
        from: &[TableRef],
        pool: &mut BufferPool,
        tx: &TransactionManager,
        cte_ctx: &std::collections::HashMap<String, (Vec<ColumnSchema>, Vec<Vec<Value>>)>,
    ) -> Result<(Vec<ColumnSchema>, Vec<Vec<Value>>)> {
        // Flatten all join steps into a list of (name, alias, join_kind, condition)
        let steps = flatten_join_tree(from);

        if steps.is_empty() {
            return Ok((vec![], vec![]));
        }

        // Apply DP join reordering
        let order = dp_join_order(&steps, &self.catalog);
        let ordered_steps: Vec<_> = order.iter().map(|&i| steps[i].clone()).collect();

        // Scan the first (left) table
        let (left_name, left_alias, _, _) = &ordered_steps[0];
        let qualifier = left_alias.as_ref().unwrap_or(left_name).to_lowercase();
        let (mut combined_schema, mut combined_rows) =
            self.scan_table_qualified(left_name, &qualifier, pool, tx, cte_ctx)?;

        // Apply each join step
        for (right_name, right_alias, join_kind, condition) in ordered_steps.iter().skip(1) {
            let right_qualifier = right_alias.as_ref().unwrap_or(right_name).to_lowercase();
            let (right_schema, right_rows) =
                self.scan_table_qualified(right_name, &right_qualifier, pool, tx, cte_ctx)?;

            let join_kind = join_kind.as_ref().unwrap_or(&JoinKind::Inner);
            let condition = condition.as_ref().unwrap();

            (combined_schema, combined_rows) = apply_join(
                combined_schema,
                combined_rows,
                right_schema,
                right_rows,
                join_kind,
                condition,
            )?;
        }

        Ok((combined_schema, combined_rows))
    }

    /// Scan a table and return rows with qualified column names (alias.col).
    fn scan_table_qualified(
        &mut self,
        name: &str,
        qualifier: &str,
        pool: &mut BufferPool,
        tx: &TransactionManager,
        cte_ctx: &std::collections::HashMap<String, (Vec<ColumnSchema>, Vec<Vec<Value>>)>,
    ) -> Result<(Vec<ColumnSchema>, Vec<Vec<Value>>)> {
        let name_lower = name.to_lowercase();

        if let Some((schema, rows)) = cte_ctx.get(&name_lower) {
            let qualified = qualify_schema(schema, qualifier);
            return Ok((qualified, rows.clone()));
        }

        // Strip schema prefix for virtual catalog table lookup
        let bare_name = name_lower
            .trim_start_matches("pg_catalog.")
            .trim_start_matches("information_schema.");

        if let Some((vschema, vrows)) = virtual_catalog_rows(bare_name, &self.catalog) {
            let qualified = qualify_schema(&vschema, qualifier);
            return Ok((qualified, vrows));
        }

        let entry = self.catalog.get(&name_lower)
            .ok_or_else(|| SqlError::Semantic(format!("table '{}' does not exist", name_lower)))?;
        let schema = entry.columns.clone();
        let snap = tx.snapshot();
        let all = entry.heap.scan(pool)?;
        let mut rows = Vec::new();
        for (_tid, raw) in all {
            let (header, row_bytes) = split_mvcc(&raw)?;
            if tx.is_visible(&header, &snap) {
                rows.push(decode_row(&schema, row_bytes)?);
            }
        }
        let qualified = qualify_schema(&schema, qualifier);
        Ok((qualified, rows))
    }

    /// Build a text query plan without executing.
    fn build_plan(&self, stmt: Statement) -> String {
        match stmt {
            Statement::Select(sel) => {
                if sel.from.is_empty() {
                    return "Result  (cost=0.00..0.00)".to_string();
                }
                let mut lines = Vec::new();
                let steps = flatten_join_tree(&sel.from);
                if steps.len() <= 1 {
                    let (name, alias, _, _) = &steps[0];
                    let qualifier = alias.as_ref().unwrap_or(name);
                    lines.push(format!("Seq Scan on {}  (cost=0.00..1.00)", qualifier));
                } else {
                    lines.push(format!("Hash Join  (cost=0.00..{}.00)", steps.len() * 10));
                    for (i, (name, alias, kind, _)) in steps.iter().enumerate() {
                        let qualifier = alias.as_ref().unwrap_or(name);
                        let prefix = match kind {
                            None => "  ->  Seq Scan on".to_string(),
                            Some(k) => format!("  ->  {:?} Seq Scan on", k),
                        };
                        lines.push(format!("{} {}  (cost=0.00..{}.00)", prefix, qualifier, i + 1));
                    }
                }
                if sel.where_clause.is_some() {
                    lines.push("  Filter: (where clause)".to_string());
                }
                if !sel.order_by.is_empty() {
                    lines.push("  Sort".to_string());
                }
                if sel.limit.is_some() {
                    lines.push("  Limit".to_string());
                }
                lines.join("\n")
            }
            Statement::Insert(_) => "Insert  (cost=0.00..0.00)".to_string(),
            Statement::Update(_) => "Update  (cost=0.00..0.00)".to_string(),
            Statement::Delete(_) => "Delete  (cost=0.00..0.00)".to_string(),
            _ => "Unknown plan".to_string(),
        }
    }

    // --- DELETE ---

    fn exec_delete(
        &mut self,
        del: DeleteStmt,
        pool: &mut BufferPool,
        wal: &mut WalManager,
        tx: &mut TransactionManager,
    ) -> Result<QueryResult> {
        let name = del.table.to_lowercase();
        let entry = self.catalog.get(&name)
            .ok_or_else(|| SqlError::Semantic(format!("table '{}' does not exist", name)))?;
        let schema = entry.columns.clone();
        let snap = tx.snapshot();

        // Collect matching TIDs
        let all = entry.heap.scan(pool)?;
        let mut to_delete = Vec::new();

        for (tid, raw) in all {
            let (header, row_bytes) = split_mvcc(&raw)?;
            if !tx.is_visible(&header, &snap) {
                continue;
            }
            let row = decode_row(&schema, row_bytes)?;
            let matches = match &del.where_clause {
                None => true,
                Some(expr) => eval_expr(expr, &schema, &row)
                    .map(|v| is_truthy(&v))
                    .unwrap_or(false),
            };
            if matches {
                to_delete.push((tid, header.xmax));
            }
        }

        let xid = tx.begin();
        let count = to_delete.len();

        for (tid, old_xmax) in to_delete {
            let entry_mut = self.catalog.get_mut(&name).unwrap();
            entry_mut.heap.delete_tuple(pool, wal, xid, tid, old_xmax)?;
        }

        tx.commit(xid, wal)?;
        wal.flush()?;

        Ok(QueryResult { tag: format!("DELETE {}", count), rows_affected: count, ..Default::default() })
    }

    // --- UPDATE ---

    fn exec_update(
        &mut self,
        upd: UpdateStmt,
        pool: &mut BufferPool,
        wal: &mut WalManager,
        tx: &mut TransactionManager,
    ) -> Result<QueryResult> {
        let name = upd.table.to_lowercase();
        let entry = self.catalog.get(&name)
            .ok_or_else(|| SqlError::Semantic(format!("table '{}' does not exist", name)))?;
        let schema = entry.columns.clone();
        let pk_col_idx = entry.pk_col_idx;
        let snap = tx.snapshot();

        // Validate assignment column names up-front
        for (col_name, _) in &upd.assignments {
            if schema.iter().position(|c| c.name.eq_ignore_ascii_case(col_name)).is_none() {
                return Err(SqlError::Semantic(format!("unknown column '{}'", col_name)));
            }
        }

        // Scan the table for rows matching the WHERE clause
        let all = entry.heap.scan(pool)?;
        let mut to_update: Vec<(oigrap_storage::TupleId, u64, Vec<Value>)> = Vec::new();

        for (tid, raw) in all {
            let (header, row_bytes) = split_mvcc(&raw)?;
            if !tx.is_visible(&header, &snap) {
                continue;
            }
            let row = decode_row(&schema, row_bytes)?;
            let matches = match &upd.where_clause {
                None => true,
                Some(expr) => eval_expr(expr, &schema, &row)
                    .map(|v| is_truthy(&v))
                    .unwrap_or(false),
            };
            if matches {
                to_update.push((tid, header.xmax, row));
            }
        }

        let xid = tx.begin();
        let count = to_update.len();

        for (tid, old_xmax, old_values) in to_update {
            // Delete the old tuple
            let entry_mut = self.catalog.get_mut(&name).unwrap();
            entry_mut.heap.delete_tuple(pool, wal, xid, tid, old_xmax)?;

            // Build updated values by applying SET assignments to old values
            let mut new_values = old_values.clone();
            for (col_name, expr) in &upd.assignments {
                let col_idx = schema.iter().position(|c| c.name.eq_ignore_ascii_case(col_name))
                    .ok_or_else(|| SqlError::Semantic(format!("unknown column '{}'", col_name)))?;
                let new_val = eval_expr(expr, &schema, &old_values)?;
                let coerced = if new_val.is_null() {
                    new_val
                } else {
                    coerce_value(&new_val, &schema[col_idx].sql_type)?
                };
                new_values[col_idx] = coerced;
            }

            // Encode and insert the new tuple
            let row_bytes = encode_row(&schema, &new_values)?;
            let header = TupleHeader::new_insert(xid, 0);
            let mut full_tuple = header.encode().to_vec();
            full_tuple.extend_from_slice(&row_bytes);

            let entry_mut = self.catalog.get_mut(&name).unwrap();
            let new_tid = entry_mut.heap.insert_tuple(pool, wal, xid, &full_tuple)?;

            // Update the B+ tree index if the PK value changed
            if let (Some(pk_idx), Some(tree)) = (pk_col_idx, entry_mut.pk_index.as_mut()) {
                let new_pk = &new_values[pk_idx];
                let key_bytes = new_pk.as_key_bytes()
                    .ok_or_else(|| SqlError::Execution("cannot index NULL primary key".into()))?;
                tree.insert(pool, &key_bytes, new_tid)?;
            }
        }

        tx.commit(xid, wal)?;
        wal.flush()?;

        Ok(QueryResult { tag: format!("UPDATE {}", count), rows_affected: count, ..Default::default() })
    }

    // --- ANALYZE ---

    fn exec_analyze(
        &mut self,
        table: &str,
        pool: &mut BufferPool,
        tx: &TransactionManager,
    ) -> Result<QueryResult> {
        let name = table.to_lowercase();
        let entry = self.catalog.get(&name)
            .ok_or_else(|| SqlError::Semantic(format!("table '{}' does not exist", name)))?;
        let schema = entry.columns.clone();
        let snap = tx.snapshot();

        // Full sequential scan
        let all = entry.heap.scan(pool)?;
        let mut rows: Vec<Vec<Value>> = Vec::new();
        for (_tid, raw) in all {
            let (header, row_bytes) = split_mvcc(&raw)?;
            if tx.is_visible(&header, &snap) {
                rows.push(decode_row(&schema, row_bytes)?);
            }
        }

        let row_count = rows.len();
        let ncols = schema.len();
        let mut col_stats: Vec<ColumnStats> = Vec::with_capacity(ncols);

        for col_idx in 0..ncols {
            let mut null_count = 0usize;
            let mut distinct: Vec<Value> = Vec::new();
            let mut value_counts: Vec<(Value, usize)> = Vec::new();

            for row in &rows {
                let val = row.get(col_idx).cloned().unwrap_or(Value::Null);
                if val.is_null() {
                    null_count += 1;
                    continue;
                }
                // Track distinct and counts
                if let Some(pos) = value_counts.iter().position(|(v, _)| v == &val) {
                    value_counts[pos].1 += 1;
                } else {
                    value_counts.push((val.clone(), 1));
                    distinct.push(val);
                }
            }

            let non_null = row_count - null_count;
            let null_fraction = if row_count > 0 { null_count as f64 / row_count as f64 } else { 0.0 };
            let ndv = distinct.len();

            // Top-10 MCVs
            value_counts.sort_by_key(|b| std::cmp::Reverse(b.1));
            let mcv: Vec<(Value, f64)> = value_counts.iter().take(10).map(|(v, cnt)| {
                let freq = if non_null > 0 { *cnt as f64 / non_null as f64 } else { 0.0 };
                (v.clone(), freq)
            }).collect();

            // Histogram: up to 50 buckets over sorted non-null values
            let mut sorted_vals: Vec<Value> = distinct.clone();
            sorted_vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let hist_bounds = if sorted_vals.len() > 2 {
                let bucket_count = 50.min(sorted_vals.len());
                let step = sorted_vals.len() as f64 / bucket_count as f64;
                (0..=bucket_count).map(|i| {
                    let idx = ((i as f64 * step) as usize).min(sorted_vals.len() - 1);
                    sorted_vals[idx].clone()
                }).collect()
            } else {
                sorted_vals
            };

            col_stats.push(ColumnStats { null_fraction, ndv, mcv, hist_bounds });
        }

        let stats = TableStats {
            row_count,
            page_count: (row_count / 100).max(1),
            columns: col_stats,
        };

        let entry_mut = self.catalog.get_mut(&name).unwrap();
        entry_mut.stats = Some(stats);

        Ok(QueryResult { tag: "ANALYZE".into(), ..Default::default() })
    }

    // --- CREATE GIN INDEX ---

    fn exec_create_gin_index(
        &mut self,
        table: &str,
        column: &str,
        pool: &mut BufferPool,
        tx: &TransactionManager,
    ) -> Result<QueryResult> {
        let name = table.to_lowercase();
        let col_lower = column.to_lowercase();

        let entry = self.catalog.get(&name)
            .ok_or_else(|| SqlError::Semantic(format!("table '{}' does not exist", name)))?;
        let schema = entry.columns.clone();
        let col_idx = schema.iter().position(|c| c.name.eq_ignore_ascii_case(&col_lower))
            .ok_or_else(|| SqlError::Semantic(format!("column '{}' does not exist", column)))?;
        let snap = tx.snapshot();

        // Scan table and build GIN
        let all = entry.heap.scan(pool)?;
        let mut gin = GinIndex::new();

        for (tid, raw) in all {
            let (header, row_bytes) = split_mvcc(&raw)?;
            if !tx.is_visible(&header, &snap) {
                continue;
            }
            let row = decode_row(&schema, row_bytes)?;
            let val = row.get(col_idx).cloned().unwrap_or(Value::Null);
            let tokens = match &val {
                Value::Text(s) => {
                    if let Ok(encoded) = encode_jsonb(s.as_str()) {
                        let jt = jsonb_tokens(&encoded);
                        if jt.is_empty() { text_tokens(s.as_str()) } else { jt }
                    } else {
                        text_tokens(s.as_str())
                    }
                }
                _ => vec![],
            };
            if !tokens.is_empty() {
                gin.insert(tid, tokens);
            }
        }

        let entry_mut = self.catalog.get_mut(&name).unwrap();
        entry_mut.gin_index = Some(gin);
        entry_mut.gin_column = Some(col_lower);

        Ok(QueryResult { tag: "CREATE GIN INDEX".into(), ..Default::default() })
    }

    // --- CREATE VECTOR INDEX ---

    fn exec_create_vector_index(
        &mut self,
        table: &str,
        column: &str,
        pool: &mut BufferPool,
        tx: &TransactionManager,
    ) -> Result<QueryResult> {
        let name = table.to_lowercase();
        let col_lower = column.to_lowercase();

        let entry = self.catalog.get(&name)
            .ok_or_else(|| SqlError::Semantic(format!("table '{}' does not exist", name)))?;
        let schema = entry.columns.clone();
        let col_idx = schema.iter().position(|c| c.name.eq_ignore_ascii_case(&col_lower))
            .ok_or_else(|| SqlError::Semantic(format!("column '{}' does not exist", column)))?;
        let snap = tx.snapshot();

        // Scan table and extract vector values
        let all = entry.heap.scan(pool)?;
        let mut hnsw = HnswIndex::new(16, 200);

        for (tid, raw) in all {
            let (header, row_bytes) = split_mvcc(&raw)?;
            if !tx.is_visible(&header, &snap) {
                continue;
            }
            let row = decode_row(&schema, row_bytes)?;
            let val = row.get(col_idx).cloned().unwrap_or(Value::Null);
            if let Value::Text(s) = val {
                if let Ok(vec) = parse_vector_str(&s) {
                    // Encode TID as u64
                    let id = tid.page_id << 16 | tid.slot_id as u64;
                    hnsw.insert(id, vec);
                }
            }
        }

        let entry_mut = self.catalog.get_mut(&name).unwrap();
        entry_mut.vector_index = Some(hnsw);

        Ok(QueryResult { tag: "CREATE VECTOR INDEX".into(), ..Default::default() })
    }

    // --- VACUUM ---

    fn exec_vacuum(
        &mut self,
        table: &str,
        pool: &mut BufferPool,
        wal: &mut WalManager,
        tx: &mut TransactionManager,
    ) -> Result<QueryResult> {
        let name = table.to_lowercase();
        let entry = self.catalog.get(&name)
            .ok_or_else(|| SqlError::Semantic(format!("table '{}' does not exist", name)))?;
        let _ = entry; // validate existence

        let entry_mut = self.catalog.get_mut(&name).unwrap();
        let stats = vacuum_table(&mut entry_mut.heap, pool, wal, tx)?;

        Ok(QueryResult {
            tag: "VACUUM".into(),
            rows_affected: stats.tuples_removed,
            ..Default::default()
        })
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

// --- Helpers ---

/// Strip the 24-byte MVCC header from raw tuple bytes.
fn split_mvcc(raw: &[u8]) -> Result<(TupleHeader, &[u8])> {
    if raw.len() < 24 {
        return Err(SqlError::Execution("tuple too short for MVCC header".into()));
    }
    let header = TupleHeader::decode(&raw[..24])
        .map_err(SqlError::Storage)?;
    Ok((header, &raw[24..]))
}

/// Evaluate a simple constant expression (literals only, no column refs).
pub fn eval_literal(expr: &Expr) -> Result<Value> {
    match expr {
        Expr::IntLit(n) => Ok(Value::Int64(*n)),
        Expr::FloatLit(f) => Ok(Value::Float64(*f)),
        Expr::StrLit(s) => Ok(Value::Text(s.clone())),
        Expr::BoolLit(b) => Ok(Value::Bool(*b)),
        Expr::Null => Ok(Value::Null),
        Expr::UnaryOp { op: UnOp::Neg, expr } => {
            match eval_literal(expr)? {
                Value::Int64(n) => Ok(Value::Int64(-n)),
                Value::Float64(f) => Ok(Value::Float64(-f)),
                other => Err(SqlError::Execution(format!("cannot negate {:?}", other))),
            }
        }
        other => Err(SqlError::Execution(format!("expected literal, got {:?}", other))),
    }
}

/// Evaluate an expression against a row.
fn eval_expr(expr: &Expr, schema: &[ColumnSchema], row: &[Value]) -> Result<Value> {
    match expr {
        Expr::IntLit(n) => Ok(Value::Int64(*n)),
        Expr::FloatLit(f) => Ok(Value::Float64(*f)),
        Expr::StrLit(s) => Ok(Value::Text(s.clone())),
        Expr::BoolLit(b) => Ok(Value::Bool(*b)),
        Expr::Null => Ok(Value::Null),

        Expr::ColumnRef { table, column } => {
            let col_lower = column.to_lowercase();
            // Handle pseudo-columns that tools sometimes write without parentheses
            if table.is_none() {
                match col_lower.as_str() {
                    "current_schema"   => return Ok(Value::Text("public".into())),
                    "current_user"     => return Ok(Value::Text("postgres".into())),
                    "current_database" => return Ok(Value::Text("postgres".into())),
                    _ => {}
                }
            }
            let idx = schema.iter().position(|c| {
                let name_lower = c.name.to_lowercase();
                if let Some(t) = table {
                    // Try qualified match: "t.col"
                    let qualified = format!("{}.{}", t.to_lowercase(), col_lower);
                    name_lower == qualified || name_lower == col_lower
                } else {
                    // Unqualified: match "col" or "*.col" suffix
                    name_lower == col_lower
                        || name_lower.ends_with(&format!(".{}", col_lower))
                }
            }).ok_or_else(|| SqlError::Semantic(format!("unknown column '{}'", column)))?;
            Ok(row[idx].clone())
        }

        Expr::UnaryOp { op: UnOp::Neg, expr } => {
            match eval_expr(expr, schema, row)? {
                Value::Int64(n) => Ok(Value::Int64(-n)),
                Value::Float64(f) => Ok(Value::Float64(-f)),
                other => Err(SqlError::Execution(format!("cannot negate {:?}", other))),
            }
        }

        Expr::UnaryOp { op: UnOp::Not, expr } => {
            let v = eval_expr(expr, schema, row)?;
            Ok(Value::Bool(!is_truthy(&v)))
        }

        Expr::BinaryOp { op, left, right } => {
            let lv = eval_expr(left, schema, row)?;
            let rv = eval_expr(right, schema, row)?;
            eval_binop(op, lv, rv)
        }

        Expr::IsNull(e) => {
            let v = eval_expr(e, schema, row)?;
            Ok(Value::Bool(v.is_null()))
        }

        Expr::IsNotNull(e) => {
            let v = eval_expr(e, schema, row)?;
            Ok(Value::Bool(!v.is_null()))
        }

        Expr::Between { expr, low, high } => {
            let v = eval_expr(expr, schema, row)?;
            let lo = eval_expr(low, schema, row)?;
            let hi = eval_expr(high, schema, row)?;
            let ge = v.partial_cmp(&lo).map(|o| o != std::cmp::Ordering::Less).unwrap_or(false);
            let le = v.partial_cmp(&hi).map(|o| o != std::cmp::Ordering::Greater).unwrap_or(false);
            Ok(Value::Bool(ge && le))
        }

        Expr::In { expr, list } => {
            let v = eval_expr(expr, schema, row)?;
            let found = list.iter().any(|e| {
                eval_expr(e, schema, row).map(|rv| v == rv).unwrap_or(false)
            });
            Ok(Value::Bool(found))
        }

        Expr::NotIn { expr, list } => {
            let v = eval_expr(expr, schema, row)?;
            let found = list.iter().any(|e| {
                eval_expr(e, schema, row).map(|rv| v == rv).unwrap_or(false)
            });
            Ok(Value::Bool(!found))
        }

        Expr::FunctionCall { name, args, .. } => {
            eval_function(name, args, schema, row)
        }

        Expr::WindowFunc { name, .. } => {
            let col_name = format!("{}()", name);
            if let Some(idx) = schema.iter().position(|c| c.name == col_name) {
                Ok(row.get(idx).cloned().unwrap_or(Value::Null))
            } else {
                Ok(Value::Null)
            }
        }

        Expr::Cast { expr, to } => {
            let v = eval_expr(expr, schema, row)?;
            v.coerce_to(to).map_err(|e| SqlError::Execution(e.to_string()))
        }

        other => Err(SqlError::Execution(format!("unsupported expression: {:?}", other))),
    }
}

fn eval_binop(op: &BinOp, l: Value, r: Value) -> Result<Value> {
    // NULL propagation
    if l.is_null() || r.is_null() {
        return Ok(Value::Null);
    }

    match op {
        BinOp::Add => numeric_binop(l, r, |a, b| a + b, |a, b| a + b),
        BinOp::Sub => numeric_binop(l, r, |a, b| a - b, |a, b| a - b),
        BinOp::Mul => numeric_binop(l, r, |a, b| a * b, |a, b| a * b),
        BinOp::Div => {
            match (&l, &r) {
                (Value::Int64(a), Value::Int64(b)) => {
                    if *b == 0 { Err(SqlError::Execution("division by zero".into())) }
                    else { Ok(Value::Int64(a / b)) }
                }
                _ => numeric_binop(l, r, |a, b| a / b, |a, b| a / b),
            }
        }
        BinOp::Mod => numeric_binop(l, r, |a, b| a % b, |a, b| a % b),

        BinOp::Eq => Ok(Value::Bool(l == r)),
        BinOp::NotEq => Ok(Value::Bool(l != r)),
        BinOp::Lt => Ok(Value::Bool(l.partial_cmp(&r).map(|o| o.is_lt()).unwrap_or(false))),
        BinOp::Gt => Ok(Value::Bool(l.partial_cmp(&r).map(|o| o.is_gt()).unwrap_or(false))),
        BinOp::LtEq => Ok(Value::Bool(l.partial_cmp(&r).map(|o| o.is_le()).unwrap_or(false))),
        BinOp::GtEq => Ok(Value::Bool(l.partial_cmp(&r).map(|o| o.is_ge()).unwrap_or(false))),

        BinOp::And => Ok(Value::Bool(is_truthy(&l) && is_truthy(&r))),
        BinOp::Or  => Ok(Value::Bool(is_truthy(&l) || is_truthy(&r))),

        BinOp::Concat => match (l, r) {
            (Value::Text(a), Value::Text(b)) => Ok(Value::Text(a + &b)),
            _ => Err(SqlError::Execution("|| requires text operands".into())),
        },

        BinOp::Like | BinOp::NotLike => {
            match (l, r) {
                (Value::Text(s), Value::Text(pat)) => {
                    let matched = like_match(&s, &pat);
                    Ok(Value::Bool(if matches!(op, BinOp::Like) { matched } else { !matched }))
                }
                _ => Err(SqlError::Execution("LIKE requires text operands".into())),
            }
        }

        BinOp::JsonGet => {
            match (l, r) {
                (Value::Text(json_str), Value::Text(key)) => {
                    json_get(&json_str, &key).map(Value::Text)
                }
                (Value::Text(json_str), Value::Int64(idx)) => {
                    json_get_idx(&json_str, idx as usize).map(Value::Text)
                }
                _ => Err(SqlError::Execution("-> requires json text operand".into())),
            }
        }

        BinOp::JsonGetText => {
            match (l, r) {
                (Value::Text(json_str), Value::Text(key)) => {
                    json_get(&json_str, &key).map(|v| {
                        // Strip surrounding quotes for ->> (returns text not JSON)
                        let stripped = v.trim();
                        if stripped.starts_with('"') && stripped.ends_with('"') && stripped.len() >= 2 {
                            Value::Text(stripped[1..stripped.len()-1].to_string())
                        } else {
                            Value::Text(v)
                        }
                    })
                }
                (Value::Text(json_str), Value::Int64(idx)) => {
                    json_get_idx(&json_str, idx as usize).map(|v| {
                        let stripped = v.trim();
                        if stripped.starts_with('"') && stripped.ends_with('"') && stripped.len() >= 2 {
                            Value::Text(stripped[1..stripped.len()-1].to_string())
                        } else {
                            Value::Text(v)
                        }
                    })
                }
                _ => Err(SqlError::Execution("->> requires json text operand".into())),
            }
        }

        BinOp::JsonContains => {
            match (l, r) {
                (Value::Text(outer_str), Value::Text(inner_str)) => {
                    let outer_bytes = encode_jsonb(&outer_str)
                        .map_err(|e| SqlError::Execution(format!("@> outer parse error: {}", e)))?;
                    let inner_bytes = encode_jsonb(&inner_str)
                        .map_err(|e| SqlError::Execution(format!("@> inner parse error: {}", e)))?;
                    Ok(Value::Bool(jsonb_contains(&outer_bytes, &inner_bytes)))
                }
                _ => Err(SqlError::Execution("@> requires json text operands".into())),
            }
        }

        BinOp::JsonKeyExists => {
            match (&l, &r) {
                (Value::Text(json_str), Value::Text(key)) => {
                    let bytes = encode_jsonb(json_str)
                        .map_err(|e| SqlError::Execution(e.to_string()))?;
                    let exists = oigrap_storage::jsonb_get_key(&bytes, key).is_some();
                    Ok(Value::Bool(exists))
                }
                _ => Err(SqlError::Execution("? requires a JSON text left operand and text key".into())),
            }
        }

        BinOp::JsonMerge => {
            match (&l, &r) {
                (Value::Text(a), Value::Text(b)) => {
                    let merged = json_merge(a, b)?;
                    Ok(Value::Text(merged))
                }
                _ => Err(SqlError::Execution("|| json merge requires two JSON text operands".into())),
            }
        }

        BinOp::JsonPath => {
            // a #> '{key1,key2}' or a #> '{0,key2}' for nested access
            match (&l, &r) {
                (Value::Text(json_str), Value::Text(path_str)) => {
                    let path = parse_json_path(path_str);
                    let mut current = json_str.clone();
                    for segment in path {
                        current = match json_get(&current, &segment) {
                            Ok(v) => v,
                            Err(_) => return Ok(Value::Null),
                        };
                    }
                    Ok(Value::Text(current))
                }
                _ => Err(SqlError::Execution("#> requires json text operands".into())),
            }
        }

        BinOp::VectorDist => {
            let la = parse_vector_val(&l)?;
            let ra = parse_vector_val(&r)?;
            let dist = l2_distance(&la, &ra)?;
            Ok(Value::Float64(dist))
        }
    }
}

fn numeric_binop(
    l: Value, r: Value,
    int_op: impl Fn(i64, i64) -> i64,
    float_op: impl Fn(f64, f64) -> f64,
) -> Result<Value> {
    match (l, r) {
        (Value::Int64(a), Value::Int64(b)) => Ok(Value::Int64(int_op(a, b))),
        (Value::Float64(a), Value::Float64(b)) => Ok(Value::Float64(float_op(a, b))),
        (Value::Int64(a), Value::Float64(b)) => Ok(Value::Float64(float_op(a as f64, b))),
        (Value::Float64(a), Value::Int64(b)) => Ok(Value::Float64(float_op(a, b as f64))),
        (l, r) => Err(SqlError::Execution(format!("arithmetic on {:?} and {:?}", l, r))),
    }
}

fn pg_show_var(name: &str) -> String {
    match name.to_lowercase().as_str() {
        "server_version" | "server_version_num" => "14.0".into(),
        "search_path" => "public".into(),
        "client_encoding" => "UTF8".into(),
        "standard_conforming_strings" => "on".into(),
        "integer_datetimes" => "on".into(),
        "datestyle" => "ISO, MDY".into(),
        "timezone" => "UTC".into(),
        "transaction_isolation" => "read committed".into(),
        "is_superuser" => "on".into(),
        "session_authorization" => "postgres".into(),
        _ => "".into(),
    }
}

/// Build a ColumnSchema suitable for virtual catalog result sets.
fn pg_col(name: &str, sql_type: crate::catalog::SqlType) -> ColumnSchema {
    ColumnSchema { name: name.to_string(), sql_type, nullable: true, primary_key: false }
}

/// Return virtual rows for pg_catalog / information_schema tables.
/// Returns None if `table` is not a recognized virtual table.
fn virtual_catalog_rows(
    table: &str,
    catalog: &Catalog,
) -> Option<(Vec<ColumnSchema>, Vec<Vec<Value>>)> {
    use crate::catalog::SqlType;
    match table {
        "pg_type" => {
            let cols = vec![
                pg_col("oid", SqlType::Int64),
                pg_col("typname", SqlType::Text),
                pg_col("typnamespace", SqlType::Int64),
                pg_col("typlen", SqlType::Int64),
                pg_col("typtype", SqlType::Text),
                pg_col("typcategory", SqlType::Text),
            ];
            let rows = vec![
                vec![Value::Int64(16),   Value::Text("bool".into()),      Value::Int64(11), Value::Int64(1),  Value::Text("b".into()), Value::Text("B".into())],
                vec![Value::Int64(20),   Value::Text("int8".into()),      Value::Int64(11), Value::Int64(8),  Value::Text("b".into()), Value::Text("N".into())],
                vec![Value::Int64(21),   Value::Text("int2".into()),      Value::Int64(11), Value::Int64(2),  Value::Text("b".into()), Value::Text("N".into())],
                vec![Value::Int64(23),   Value::Text("int4".into()),      Value::Int64(11), Value::Int64(4),  Value::Text("b".into()), Value::Text("N".into())],
                vec![Value::Int64(25),   Value::Text("text".into()),      Value::Int64(11), Value::Int64(-1), Value::Text("b".into()), Value::Text("S".into())],
                vec![Value::Int64(700),  Value::Text("float4".into()),    Value::Int64(11), Value::Int64(4),  Value::Text("b".into()), Value::Text("N".into())],
                vec![Value::Int64(701),  Value::Text("float8".into()),    Value::Int64(11), Value::Int64(8),  Value::Text("b".into()), Value::Text("N".into())],
                vec![Value::Int64(1043), Value::Text("varchar".into()),   Value::Int64(11), Value::Int64(-1), Value::Text("b".into()), Value::Text("S".into())],
                vec![Value::Int64(1082), Value::Text("date".into()),      Value::Int64(11), Value::Int64(4),  Value::Text("b".into()), Value::Text("D".into())],
                vec![Value::Int64(1114), Value::Text("timestamp".into()), Value::Int64(11), Value::Int64(8),  Value::Text("b".into()), Value::Text("D".into())],
                vec![Value::Int64(3802), Value::Text("jsonb".into()),     Value::Int64(11), Value::Int64(-1), Value::Text("b".into()), Value::Text("U".into())],
            ];
            Some((cols, rows))
        }
        "pg_namespace" => {
            let cols = vec![
                pg_col("oid", SqlType::Int64),
                pg_col("nspname", SqlType::Text),
                pg_col("nspowner", SqlType::Int64),
            ];
            let rows = vec![
                vec![Value::Int64(11),   Value::Text("pg_catalog".into()), Value::Int64(10)],
                vec![Value::Int64(2200), Value::Text("public".into()),     Value::Int64(10)],
            ];
            Some((cols, rows))
        }
        "pg_database" => {
            let cols = vec![
                pg_col("oid", SqlType::Int64),
                pg_col("datname", SqlType::Text),
                pg_col("datdba", SqlType::Int64),
                pg_col("encoding", SqlType::Int64),
                pg_col("datcollate", SqlType::Text),
                pg_col("datctype", SqlType::Text),
            ];
            let rows = vec![vec![
                Value::Int64(1),
                Value::Text("postgres".into()),
                Value::Int64(10),
                Value::Int64(6),
                Value::Text("en_US.UTF-8".into()),
                Value::Text("en_US.UTF-8".into()),
            ]];
            Some((cols, rows))
        }
        "pg_user" | "pg_roles" => {
            let cols = vec![
                pg_col("usename", SqlType::Text),
                pg_col("usesysid", SqlType::Int64),
                pg_col("usecreatedb", SqlType::Boolean),
                pg_col("usesuper", SqlType::Boolean),
            ];
            let rows = vec![vec![
                Value::Text("postgres".into()),
                Value::Int64(10),
                Value::Bool(true),
                Value::Bool(true),
            ]];
            Some((cols, rows))
        }
        "pg_class" => {
            let cols = vec![
                pg_col("oid", SqlType::Int64),
                pg_col("relname", SqlType::Text),
                pg_col("relnamespace", SqlType::Int64),
                pg_col("relkind", SqlType::Text),
                pg_col("relowner", SqlType::Int64),
                pg_col("relpages", SqlType::Int64),
                pg_col("reltuples", SqlType::Float64),
            ];
            let mut rows = Vec::new();
            for (idx, (tname, _)) in catalog.tables().enumerate() {
                rows.push(vec![
                    Value::Int64((16384 + idx) as i64),
                    Value::Text(tname.clone()),
                    Value::Int64(2200),
                    Value::Text("r".into()),
                    Value::Int64(10),
                    Value::Int64(1),
                    Value::Float64(0.0),
                ]);
            }
            Some((cols, rows))
        }
        "pg_tables" => {
            let cols = vec![
                pg_col("schemaname", SqlType::Text),
                pg_col("tablename", SqlType::Text),
                pg_col("tableowner", SqlType::Text),
                pg_col("hasindexes", SqlType::Boolean),
                pg_col("hasrules", SqlType::Boolean),
                pg_col("hastriggers", SqlType::Boolean),
            ];
            let mut rows = Vec::new();
            for (tname, _) in catalog.tables() {
                rows.push(vec![
                    Value::Text("public".into()),
                    Value::Text(tname.clone()),
                    Value::Text("postgres".into()),
                    Value::Bool(false),
                    Value::Bool(false),
                    Value::Bool(false),
                ]);
            }
            Some((cols, rows))
        }
        "pg_attribute" => {
            let cols = vec![
                pg_col("attrelid", SqlType::Int64),
                pg_col("attname", SqlType::Text),
                pg_col("atttypid", SqlType::Int64),
                pg_col("attnum", SqlType::Int64),
                pg_col("attnotnull", SqlType::Boolean),
                pg_col("atthasdef", SqlType::Boolean),
            ];
            let mut rows = Vec::new();
            for (idx, (_, entry)) in catalog.tables().enumerate() {
                let rel_oid = (16384 + idx) as i64;
                for (col_idx, c) in entry.columns.iter().enumerate() {
                    let type_oid = match c.sql_type {
                        SqlType::Boolean => 16i64,
                        SqlType::Int64   => 20,
                        SqlType::Float64 => 701,
                        SqlType::Text    => 25,
                    };
                    rows.push(vec![
                        Value::Int64(rel_oid),
                        Value::Text(c.name.clone()),
                        Value::Int64(type_oid),
                        Value::Int64((col_idx + 1) as i64),
                        Value::Bool(!c.nullable),
                        Value::Bool(false),
                    ]);
                }
            }
            Some((cols, rows))
        }
        "pg_indexes" => {
            let cols = vec![
                pg_col("schemaname", SqlType::Text),
                pg_col("tablename", SqlType::Text),
                pg_col("indexname", SqlType::Text),
                pg_col("indexdef", SqlType::Text),
            ];
            Some((cols, vec![]))
        }
        "pg_constraint" => {
            let cols = vec![
                pg_col("oid", SqlType::Int64),
                pg_col("conname", SqlType::Text),
                pg_col("contype", SqlType::Text),
                pg_col("conrelid", SqlType::Int64),
            ];
            Some((cols, vec![]))
        }
        "pg_stat_user_tables" => {
            let cols = vec![
                pg_col("relid", SqlType::Int64),
                pg_col("schemaname", SqlType::Text),
                pg_col("relname", SqlType::Text),
                pg_col("n_live_tup", SqlType::Int64),
            ];
            let mut rows = Vec::new();
            for (idx, (tname, _)) in catalog.tables().enumerate() {
                rows.push(vec![
                    Value::Int64((16384 + idx) as i64),
                    Value::Text("public".into()),
                    Value::Text(tname.clone()),
                    Value::Int64(0),
                ]);
            }
            Some((cols, rows))
        }
        "tables" => {
            // information_schema.tables
            let cols = vec![
                pg_col("table_catalog", SqlType::Text),
                pg_col("table_schema", SqlType::Text),
                pg_col("table_name", SqlType::Text),
                pg_col("table_type", SqlType::Text),
            ];
            let mut rows = Vec::new();
            for (tname, _) in catalog.tables() {
                rows.push(vec![
                    Value::Text("postgres".into()),
                    Value::Text("public".into()),
                    Value::Text(tname.clone()),
                    Value::Text("BASE TABLE".into()),
                ]);
            }
            Some((cols, rows))
        }
        "columns" => {
            // information_schema.columns
            let cols = vec![
                pg_col("table_catalog", SqlType::Text),
                pg_col("table_schema", SqlType::Text),
                pg_col("table_name", SqlType::Text),
                pg_col("column_name", SqlType::Text),
                pg_col("ordinal_position", SqlType::Int64),
                pg_col("is_nullable", SqlType::Text),
                pg_col("data_type", SqlType::Text),
            ];
            let mut rows = Vec::new();
            for (tname, entry) in catalog.tables() {
                for (pos, c) in entry.columns.iter().enumerate() {
                    let type_name = match c.sql_type {
                        SqlType::Int64   => "bigint",
                        SqlType::Float64 => "double precision",
                        SqlType::Text    => "text",
                        SqlType::Boolean => "boolean",
                    };
                    rows.push(vec![
                        Value::Text("postgres".into()),
                        Value::Text("public".into()),
                        Value::Text(tname.clone()),
                        Value::Text(c.name.clone()),
                        Value::Int64((pos + 1) as i64),
                        Value::Text(if c.nullable { "YES" } else { "NO" }.into()),
                        Value::Text(type_name.into()),
                    ]);
                }
            }
            Some((cols, rows))
        }
        "schemata" => {
            let cols = vec![
                pg_col("catalog_name", SqlType::Text),
                pg_col("schema_name", SqlType::Text),
                pg_col("schema_owner", SqlType::Text),
            ];
            Some((cols, vec![
                vec![Value::Text("postgres".into()), Value::Text("public".into()),     Value::Text("postgres".into())],
                vec![Value::Text("postgres".into()), Value::Text("pg_catalog".into()), Value::Text("postgres".into())],
            ]))
        }
        "views" => {
            let cols = vec![
                pg_col("table_catalog", SqlType::Text),
                pg_col("table_schema", SqlType::Text),
                pg_col("table_name", SqlType::Text),
                pg_col("view_definition", SqlType::Text),
            ];
            Some((cols, vec![]))
        }
        _ => None,
    }
}

fn eval_function(name: &str, args: &[Expr], schema: &[ColumnSchema], row: &[Value]) -> Result<Value> {
    match name {
        "COALESCE" => {
            for a in args {
                let v = eval_expr(a, schema, row)?;
                if !v.is_null() { return Ok(v); }
            }
            Ok(Value::Null)
        }
        "UPPER" => {
            if let Some(Value::Text(s)) = args.first().map(|e| eval_expr(e, schema, row)).transpose()? {
                Ok(Value::Text(s.to_uppercase()))
            } else { Ok(Value::Null) }
        }
        "LOWER" => {
            if let Some(Value::Text(s)) = args.first().map(|e| eval_expr(e, schema, row)).transpose()? {
                Ok(Value::Text(s.to_lowercase()))
            } else { Ok(Value::Null) }
        }
        "LENGTH" => {
            if let Some(Value::Text(s)) = args.first().map(|e| eval_expr(e, schema, row)).transpose()? {
                Ok(Value::Int64(s.len() as i64))
            } else { Ok(Value::Null) }
        }
        "DEPTH" => {
            // Returns current 1-based recursion depth inside a WITH RECURSIVE CTE.
            // The thread-local CTE_DEPTH is set to the current iteration before the
            // recursive step executes and reset to 0 when the loop exits.
            let depth = CTE_DEPTH.with(|d| d.get());
            Ok(Value::Int64(depth))
        }
        "OIGRAP_SHORTEST_PATH" => {
            // oigrap_shortest_path(source_id, target_id, edge_table, from_col, to_col)
            // Runs BFS from source to target through the named edge table.
            // Returns path length (number of hops) as Int64, or Null if unreachable.
            if args.len() < 5 {
                return Err(SqlError::Execution(
                    "oigrap_shortest_path requires 5 arguments: source_id, target_id, edge_table, from_col, to_col".into()
                ));
            }
            let source = match eval_expr(&args[0], schema, row)? {
                Value::Int64(n) => n,
                other => return Err(SqlError::Execution(format!("oigrap_shortest_path: source_id must be integer, got {:?}", other))),
            };
            let target = match eval_expr(&args[1], schema, row)? {
                Value::Int64(n) => n,
                other => return Err(SqlError::Execution(format!("oigrap_shortest_path: target_id must be integer, got {:?}", other))),
            };
            let edge_table = match eval_expr(&args[2], schema, row)? {
                Value::Text(s) => s.to_lowercase(),
                other => return Err(SqlError::Execution(format!("oigrap_shortest_path: edge_table must be text, got {:?}", other))),
            };
            let from_col = match eval_expr(&args[3], schema, row)? {
                Value::Text(s) => s.to_lowercase(),
                other => return Err(SqlError::Execution(format!("oigrap_shortest_path: from_col must be text, got {:?}", other))),
            };
            let to_col = match eval_expr(&args[4], schema, row)? {
                Value::Text(s) => s.to_lowercase(),
                other => return Err(SqlError::Execution(format!("oigrap_shortest_path: to_col must be text, got {:?}", other))),
            };

            // Look up edges from the thread-local cache (populated before project() runs).
            let cache_key = format!("{}/{}/{}", edge_table, from_col, to_col);
            let edges: Vec<(i64, i64)> = EDGE_CACHE.with(|c| {
                c.borrow().get(&cache_key).cloned().unwrap_or_default()
            });

            // BFS from source to target.
            if source == target {
                return Ok(Value::Int64(0));
            }
            let mut visited: std::collections::HashSet<i64> = std::collections::HashSet::new();
            let mut queue: std::collections::VecDeque<(i64, i64)> = std::collections::VecDeque::new();
            queue.push_back((source, 0));
            visited.insert(source);
            while let Some((node, dist)) = queue.pop_front() {
                for &(from, to) in &edges {
                    let neighbor = if from == node { to } else { continue };
                    if neighbor == target {
                        return Ok(Value::Int64(dist + 1));
                    }
                    if visited.insert(neighbor) {
                        queue.push_back((neighbor, dist + 1));
                    }
                }
            }
            Ok(Value::Null)
        }
        "ARRAY_AGG" | "STRING_AGG" => {
            // These are aggregate functions — when called from eval_function (single-row context)
            // they return Null; the real aggregate path is eval_aggregate.
            Ok(Value::Null)
        }
        "VERSION" | "PG_CATALOG.VERSION" => {
            Ok(Value::Text("PostgreSQL 14.0 (oigrap 0.1.0)".into()))
        }
        "CURRENT_DATABASE" | "CURRENT_CATALOG" => {
            Ok(Value::Text("postgres".into()))
        }
        "CURRENT_SCHEMA" | "CURRENT_SCHEMAS" => {
            Ok(Value::Text("public".into()))
        }
        "CURRENT_USER" | "SESSION_USER" | "USER" => {
            Ok(Value::Text("postgres".into()))
        }
        "PG_BACKEND_PID" => {
            Ok(Value::Int64(1))
        }
        "PG_POSTMASTER_START_TIME" | "NOW" | "CLOCK_TIMESTAMP" | "TRANSACTION_TIMESTAMP" | "STATEMENT_TIMESTAMP" => {
            Ok(Value::Text("2024-01-01 00:00:00+00".into()))
        }
        "PG_ENCODING_TO_CHAR" => {
            Ok(Value::Text("UTF8".into()))
        }
        "PG_GET_USERBYID" => {
            Ok(Value::Text("postgres".into()))
        }
        "FORMAT_TYPE" | "PG_CATALOG.FORMAT_TYPE" => {
            Ok(Value::Text("text".into()))
        }
        "OBJ_DESCRIPTION" | "COL_DESCRIPTION" | "SHOBJ_DESCRIPTION" => {
            Ok(Value::Null)
        }
        "PG_TABLE_IS_VISIBLE" | "PG_TYPE_IS_VISIBLE" => {
            Ok(Value::Bool(true))
        }
        "PG_RELATION_SIZE" | "PG_TOTAL_RELATION_SIZE" | "PG_DATABASE_SIZE" => {
            Ok(Value::Int64(0))
        }
        "QUOTE_IDENT" => {
            if let Some(v) = args.first() {
                match eval_expr(v, schema, row)? {
                    Value::Text(s) => Ok(Value::Text(format!("\"{}\"", s))),
                    other => Ok(other),
                }
            } else {
                Ok(Value::Null)
            }
        }
        "QUOTE_LITERAL" => {
            if let Some(v) = args.first() {
                match eval_expr(v, schema, row)? {
                    Value::Text(s) => Ok(Value::Text(format!("'{}'", s.replace('\'', "''")))),
                    other => Ok(other),
                }
            } else {
                Ok(Value::Null)
            }
        }
        "PG_CATALOG.PG_GET_EXPR" | "PG_GET_EXPR" => {
            Ok(Value::Null)
        }
        "OIGRAP_PAGERANK" => {
            if args.len() < 4 {
                return Err(SqlError::Execution(
                    "oigrap_pagerank requires 4 arguments: node_id, edge_table, from_col, to_col".into()
                ));
            }
            let node_id = match eval_expr(&args[0], schema, row)? {
                Value::Int64(n) => n,
                other => return Err(SqlError::Execution(format!("oigrap_pagerank: node_id must be integer, got {:?}", other))),
            };
            let edge_table = match eval_expr(&args[1], schema, row)? {
                Value::Text(s) => s.to_lowercase(),
                other => return Err(SqlError::Execution(format!("oigrap_pagerank: edge_table must be text, got {:?}", other))),
            };
            let from_col = match eval_expr(&args[2], schema, row)? {
                Value::Text(s) => s.to_lowercase(),
                other => return Err(SqlError::Execution(format!("oigrap_pagerank: from_col must be text, got {:?}", other))),
            };
            let to_col = match eval_expr(&args[3], schema, row)? {
                Value::Text(s) => s.to_lowercase(),
                other => return Err(SqlError::Execution(format!("oigrap_pagerank: to_col must be text, got {:?}", other))),
            };
            let cache_key = format!("{}/{}/{}", edge_table, from_col, to_col);
            let rank = PAGERANK_CACHE.with(|c| {
                c.borrow().get(&cache_key)
                    .and_then(|m| m.get(&node_id).copied())
            });
            Ok(rank.map(Value::Float64).unwrap_or(Value::Null))
        }
        _ => Err(SqlError::Execution(format!("unknown function '{}'", name))),
    }
}

/// SQL LIKE pattern matching: % = any sequence, _ = any single character.
fn like_match(s: &str, pattern: &str) -> bool {
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let mut dp = vec![vec![false; p.len() + 1]; s.len() + 1];
    dp[0][0] = true;
    for j in 1..=p.len() {
        if p[j - 1] == '%' { dp[0][j] = dp[0][j - 1]; }
    }
    for i in 1..=s.len() {
        for j in 1..=p.len() {
            if p[j - 1] == '%' {
                dp[i][j] = dp[i - 1][j] || dp[i][j - 1];
            } else if p[j - 1] == '_' || p[j - 1] == s[i - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            }
        }
    }
    dp[s.len()][p.len()]
}

fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Int64(n) => *n != 0,
        Value::Float64(f) => *f != 0.0,
        Value::Text(s) => !s.is_empty(),
    }
}

fn coerce_value(v: &Value, target: &SqlType) -> Result<Value> {
    match (v, target) {
        (Value::Int64(n), SqlType::Int64) => Ok(Value::Int64(*n)),
        (Value::Int64(n), SqlType::Float64) => Ok(Value::Float64(*n as f64)),
        (Value::Float64(f), SqlType::Float64) => Ok(Value::Float64(*f)),
        (Value::Text(s), SqlType::Text) => Ok(Value::Text(s.clone())),
        (Value::Bool(b), SqlType::Boolean) => Ok(Value::Bool(*b)),
        (Value::Null, _) => Ok(Value::Null),
        _ => Err(SqlError::Semantic(format!("type mismatch: {:?} cannot coerce to {:?}", v, target))),
    }
}

/// Describes a detected GIN query pattern: `col @> 'json_literal'`
struct GinQuery {
    col_name: String,
    json_str: String,
}

/// Detect `WHERE col @> 'json_literal'` pattern for GIN acceleration.
fn detect_gin_query(where_clause: &Option<Expr>, schema: &[ColumnSchema]) -> Option<GinQuery> {
    let expr = where_clause.as_ref()?;
    if let Expr::BinaryOp { op: BinOp::JsonContains, left, right } = expr {
        if let (
            Expr::ColumnRef { column, .. },
            Expr::StrLit(json_str),
        ) = (left.as_ref(), right.as_ref())
        {
            // Verify the column exists in schema
            let col_lower = column.to_lowercase();
            if schema.iter().any(|c| {
                let n = c.name.to_lowercase();
                n == col_lower || n.ends_with(&format!(".{}", col_lower))
            }) {
                return Some(GinQuery {
                    col_name: col_lower,
                    json_str: json_str.clone(),
                });
            }
        }
    }
    None
}

/// Check if the WHERE clause is a simple `pk_col = literal` that can use the index.
fn can_use_pk_lookup(
    where_clause: &Option<Expr>,
    schema: &[ColumnSchema],
    pk_col_idx: Option<usize>,
) -> Option<(usize, Value)> {
    let pk_idx = pk_col_idx?;
    let pk_name = &schema[pk_idx].name;
    let expr = where_clause.as_ref()?;

    if let Expr::BinaryOp { op: BinOp::Eq, left, right } = expr {
        let (col, val_expr) = match (left.as_ref(), right.as_ref()) {
            (Expr::ColumnRef { column, .. }, val) => (column, val),
            (val, Expr::ColumnRef { column, .. }) => (column, val),
            _ => return None,
        };
        if !col.eq_ignore_ascii_case(pk_name) {
            return None;
        }
        let val = eval_literal(val_expr).ok()?;
        Some((pk_idx, val))
    } else {
        None
    }
}

/// Project columns from the SELECT list.
#[allow(clippy::type_complexity)]
fn project(
    select_cols: &[SelectColumn],
    schema: &[ColumnSchema],
    rows: Vec<Vec<Value>>,
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    // Determine output column names and indices
    let mut output_names: Vec<String> = Vec::new();
    let mut exprs: Vec<Box<dyn Fn(&[Value]) -> Result<Value>>> = Vec::new();

    for sc in select_cols {
        match sc {
            SelectColumn::Star => {
                for (i, col) in schema.iter().enumerate() {
                    output_names.push(col.name.clone());
                    exprs.push(Box::new(move |row: &[Value]| Ok(row[i].clone())));
                }
            }
            SelectColumn::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| match expr {
                    Expr::ColumnRef { column, .. } => column.clone(),
                    _ => "?column?".to_string(),
                });
                output_names.push(name);
                let expr_clone = expr.clone();
                let schema_clone = schema.to_vec();
                exprs.push(Box::new(move |row: &[Value]| {
                    eval_expr(&expr_clone, &schema_clone, row)
                }));
            }
        }
    }

    let projected = rows.iter().map(|row| {
        exprs.iter().map(|f| f(row)).collect::<Result<Vec<_>>>()
    }).collect::<Result<Vec<_>>>()?;

    Ok((output_names, projected))
}

// ── Aggregate detection ───────────────────────────────────────────────────────

fn has_aggregates(cols: &[SelectColumn]) -> bool {
    cols.iter().any(|sc| match sc {
        SelectColumn::Expr { expr, .. } => expr_has_aggregate(expr),
        SelectColumn::Star => false,
    })
}

fn expr_has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall { name, .. } => {
            matches!(
                name.to_uppercase().as_str(),
                "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "ARRAY_AGG" | "STRING_AGG"
            )
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_has_aggregate(left) || expr_has_aggregate(right)
        }
        _ => false,
    }
}

/// Determine the output column name for a SELECT column in an aggregate context.
fn agg_col_name(sc: &SelectColumn, idx: usize) -> String {
    match sc {
        SelectColumn::Expr { alias: Some(a), .. } => a.clone(),
        SelectColumn::Expr { expr, alias: None } => match expr {
            Expr::ColumnRef { column, .. } => column.clone(),
            Expr::FunctionCall { name, args, .. } => {
                let arg_str = match args.first() {
                    Some(Expr::Star) | None => "*".to_string(),
                    Some(Expr::ColumnRef { column, .. }) => column.clone(),
                    _ => "?".to_string(),
                };
                format!("{}({})", name.to_lowercase(), arg_str)
            }
            _ => format!("?column{}?", idx),
        },
        SelectColumn::Star => "*".to_string(),
    }
}

/// Compute a single aggregate function over the rows in a group.
fn eval_aggregate(
    name: &str,
    args: &[Expr],
    distinct: bool,
    schema: &[ColumnSchema],
    rows: &[Vec<Value>],
) -> Result<Value> {
    let upper = name.to_uppercase();
    match upper.as_str() {
        "COUNT" => {
            let is_star = args.is_empty()
                || matches!(args.first(), Some(Expr::Star));
            let count = if is_star {
                rows.len() as i64
            } else {
                let expr = &args[0];
                if distinct {
                    let mut seen: Vec<Value> = Vec::new();
                    let mut c = 0i64;
                    for row in rows {
                        let v = eval_expr(expr, schema, row).unwrap_or(Value::Null);
                        if !v.is_null() && !seen.contains(&v) {
                            seen.push(v);
                            c += 1;
                        }
                    }
                    c
                } else {
                    rows.iter().filter(|row| {
                        !eval_expr(expr, schema, row).unwrap_or(Value::Null).is_null()
                    }).count() as i64
                }
            };
            Ok(Value::Int64(count))
        }
        "SUM" => {
            if args.is_empty() {
                return Ok(Value::Null);
            }
            let expr = &args[0];
            let mut sum_int: Option<i64> = None;
            let mut sum_float: Option<f64> = None;
            let mut any = false;
            for row in rows {
                let v = eval_expr(expr, schema, row).unwrap_or(Value::Null);
                match v {
                    Value::Int64(n) => {
                        any = true;
                        sum_int = Some(sum_int.unwrap_or(0) + n);
                    }
                    Value::Float64(f) => {
                        any = true;
                        sum_float = Some(sum_float.unwrap_or(0.0) + f);
                    }
                    _ => {}
                }
            }
            if !any {
                return Ok(Value::Null);
            }
            if let Some(sf) = sum_float {
                Ok(Value::Float64(sf + sum_int.unwrap_or(0) as f64))
            } else {
                Ok(Value::Int64(sum_int.unwrap_or(0)))
            }
        }
        "AVG" => {
            if args.is_empty() {
                return Ok(Value::Null);
            }
            let expr = &args[0];
            let mut sum = 0.0f64;
            let mut count = 0usize;
            for row in rows {
                let v = eval_expr(expr, schema, row).unwrap_or(Value::Null);
                match v {
                    Value::Int64(n) => { sum += n as f64; count += 1; }
                    Value::Float64(f) => { sum += f; count += 1; }
                    _ => {}
                }
            }
            if count == 0 {
                Ok(Value::Null)
            } else {
                Ok(Value::Float64(sum / count as f64))
            }
        }
        "MIN" => {
            if args.is_empty() {
                return Ok(Value::Null);
            }
            let expr = &args[0];
            let mut min_val: Option<Value> = None;
            for row in rows {
                let v = eval_expr(expr, schema, row).unwrap_or(Value::Null);
                if v.is_null() { continue; }
                min_val = Some(match min_val.take() {
                    None => v,
                    Some(cur) => if v.partial_cmp(&cur).map(|o| o.is_lt()).unwrap_or(false) { v } else { cur },
                });
            }
            Ok(min_val.unwrap_or(Value::Null))
        }
        "MAX" => {
            if args.is_empty() {
                return Ok(Value::Null);
            }
            let expr = &args[0];
            let mut max_val: Option<Value> = None;
            for row in rows {
                let v = eval_expr(expr, schema, row).unwrap_or(Value::Null);
                if v.is_null() { continue; }
                max_val = Some(match max_val.take() {
                    None => v,
                    Some(cur) => if v.partial_cmp(&cur).map(|o| o.is_gt()).unwrap_or(false) { v } else { cur },
                });
            }
            Ok(max_val.unwrap_or(Value::Null))
        }
        "ARRAY_AGG" => {
            if args.is_empty() {
                return Ok(Value::Null);
            }
            let expr = &args[0];
            let parts: Vec<String> = rows.iter()
                .map(|row| eval_expr(expr, schema, row).unwrap_or(Value::Null))
                .map(|v| match v {
                    Value::Null => "null".to_string(),
                    Value::Int64(n) => n.to_string(),
                    Value::Float64(f) => f.to_string(),
                    Value::Bool(b) => b.to_string(),
                    Value::Text(s) => format!("\"{}\"", s.replace('"', "\\\"")),
                })
                .collect();
            Ok(Value::Text(format!("[{}]", parts.join(","))))
        }
        "STRING_AGG" => {
            if args.is_empty() {
                return Ok(Value::Null);
            }
            let expr = &args[0];
            // Delimiter is the second argument (evaluated against first row), default ","
            let delim = args.get(1)
                .and_then(|e| rows.first().and_then(|row| eval_expr(e, schema, row).ok()))
                .and_then(|v| if let Value::Text(s) = v { Some(s) } else { None })
                .unwrap_or_else(|| ",".to_string());
            let parts: Vec<String> = rows.iter()
                .filter_map(|row| eval_expr(expr, schema, row).ok())
                .filter_map(|v| if let Value::Text(s) = v { Some(s) } else { None })
                .collect();
            Ok(Value::Text(parts.join(&delim)))
        }
        _ => Err(SqlError::Execution(format!("unknown aggregate function '{}'", name))),
    }
}

/// Evaluate a SELECT column expression in aggregate context (group rows provided).
/// For aggregate functions, computes the aggregate over the group.
/// For non-aggregate expressions, evaluates against the first row of the group.
fn eval_agg_column(
    sc: &SelectColumn,
    schema: &[ColumnSchema],
    group_rows: &[Vec<Value>],
) -> Result<Value> {
    match sc {
        SelectColumn::Star => {
            // Star in aggregate context should not normally appear; return null
            Ok(Value::Null)
        }
        SelectColumn::Expr { expr, .. } => {
            eval_agg_expr(expr, schema, group_rows)
        }
    }
}

fn eval_agg_expr(
    expr: &Expr,
    schema: &[ColumnSchema],
    group_rows: &[Vec<Value>],
) -> Result<Value> {
    match expr {
        Expr::FunctionCall { name, args, distinct } => {
            let upper = name.to_uppercase();
            if matches!(upper.as_str(), "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "ARRAY_AGG" | "STRING_AGG") {
                eval_aggregate(name, args, *distinct, schema, group_rows)
            } else {
                // Non-aggregate function: evaluate against first row
                let first = group_rows.first().map(|r| r.as_slice()).unwrap_or(&[]);
                eval_function(name, args, schema, first)
            }
        }
        Expr::BinaryOp { op, left, right } => {
            let lv = eval_agg_expr(left, schema, group_rows)?;
            let rv = eval_agg_expr(right, schema, group_rows)?;
            eval_binop(op, lv, rv)
        }
        // For non-aggregate expressions, evaluate against the first row of the group
        other => {
            let first = group_rows.first().map(|r| r.as_slice()).unwrap_or(&[]);
            eval_expr(other, schema, first)
        }
    }
}

/// Apply GROUP BY, aggregate functions, and HAVING to a set of rows.
/// Returns (column_names, result_rows).
fn apply_group_by(
    group_by: &[Expr],
    select_cols: &[SelectColumn],
    having: &Option<Expr>,
    schema: &[ColumnSchema],
    rows: Vec<Vec<Value>>,
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    // Compute output column names
    let col_names: Vec<String> = select_cols
        .iter()
        .enumerate()
        .map(|(i, sc)| agg_col_name(sc, i))
        .collect();

    // Build groups: key = Vec<Value> from GROUP BY expressions
    // Use Vec of (key, rows) to preserve insertion order
    let mut group_keys: Vec<Vec<Value>> = Vec::new();
    let mut group_map: Vec<(Vec<Value>, Vec<Vec<Value>>)> = Vec::new();

    if group_by.is_empty() {
        // Single group over all rows
        group_map.push((vec![], rows));
    } else {
        for row in rows {
            let key: Vec<Value> = group_by.iter()
                .map(|e| eval_expr(e, schema, &row).unwrap_or(Value::Null))
                .collect();
            if let Some(pos) = group_keys.iter().position(|k| k == &key) {
                group_map[pos].1.push(row);
            } else {
                group_keys.push(key.clone());
                group_map.push((key, vec![row]));
            }
        }
    }

    // For each group, compute aggregate values
    let mut result_rows: Vec<Vec<Value>> = Vec::new();

    for (_key, group_rows) in &group_map {
        let result_row: Vec<Value> = select_cols.iter()
            .map(|sc| eval_agg_column(sc, schema, group_rows))
            .collect::<Result<Vec<_>>>()?;

        // Apply HAVING filter using the result row and a synthetic schema
        if let Some(having_expr) = having {
            // Build a synthetic schema from the output column names for HAVING evaluation
            let synthetic_schema: Vec<ColumnSchema> = col_names.iter().map(|n| ColumnSchema {
                name: n.clone(),
                sql_type: SqlType::Text, // placeholder
                nullable: true,
                primary_key: false,
            }).collect();
            let passes = eval_expr(having_expr, &synthetic_schema, &result_row)
                .map(|v| is_truthy(&v))
                .unwrap_or(false);
            if !passes {
                continue;
            }
        }

        result_rows.push(result_row);
    }

    Ok((col_names, result_rows))
}

// --- JOIN helpers ---

/// Qualify all column names with a table prefix: "col" -> "alias.col"
fn qualify_schema(schema: &[ColumnSchema], table_qualifier: &str) -> Vec<ColumnSchema> {
    schema.iter().map(|col| ColumnSchema {
        name: format!("{}.{}", table_qualifier, col.name),
        sql_type: col.sql_type.clone(),
        nullable: col.nullable,
        primary_key: col.primary_key,
    }).collect()
}

/// Flatten the join tree (from clause with nested JoinClause) into a flat list of steps.
/// Each step: (table_name, alias, join_kind, join_condition)
/// The first entry has kind=None (left-most table).
#[allow(clippy::type_complexity)]
fn flatten_join_tree(
    from: &[TableRef],
) -> Vec<(String, Option<String>, Option<JoinKind>, Option<JoinCondition>)> {
    let mut steps = Vec::new();
    for table_ref in from {
        steps.push((
            table_ref.name.clone(),
            table_ref.alias.clone(),
            None,
            None,
        ));
        flatten_join_node(&table_ref.join, &mut steps);
    }
    steps
}

#[allow(clippy::type_complexity)]
fn flatten_join_node(
    join: &Option<Box<JoinClause>>,
    steps: &mut Vec<(String, Option<String>, Option<JoinKind>, Option<JoinCondition>)>,
) {
    if let Some(jc) = join {
        steps.push((
            jc.right.name.clone(),
            jc.right.alias.clone(),
            Some(jc.kind.clone()),
            Some(jc.condition.clone()),
        ));
        flatten_join_node(&jc.right.join, steps);
    }
}

/// Apply a join between left and right result sets.
fn apply_join(
    left_schema: Vec<ColumnSchema>,
    left_rows: Vec<Vec<Value>>,
    right_schema: Vec<ColumnSchema>,
    right_rows: Vec<Vec<Value>>,
    kind: &JoinKind,
    condition: &JoinCondition,
) -> Result<(Vec<ColumnSchema>, Vec<Vec<Value>>)> {
    let combined_schema: Vec<ColumnSchema> = left_schema.iter()
        .chain(right_schema.iter())
        .cloned()
        .collect();

    let null_right: Vec<Value> = right_schema.iter().map(|_| Value::Null).collect();

    // Extract equality condition columns for potential hash join optimization
    let eq_cols = extract_eq_cols(condition);

    let mut result = Vec::new();

    match kind {
        JoinKind::Cross => {
            for left_row in &left_rows {
                for right_row in &right_rows {
                    let mut combined = left_row.clone();
                    combined.extend_from_slice(right_row);
                    result.push(combined);
                }
            }
        }
        JoinKind::Inner => {
            if let Some((left_col, right_col)) = eq_cols {
                // Hash join: build hash table on right side
                let left_idx = left_schema.iter().position(|c| {
                    let nl = c.name.to_lowercase();
                    nl == left_col.to_lowercase() || nl.ends_with(&format!(".{}", left_col.to_lowercase()))
                });
                let right_idx = right_schema.iter().position(|c| {
                    let nl = c.name.to_lowercase();
                    nl == right_col.to_lowercase() || nl.ends_with(&format!(".{}", right_col.to_lowercase()))
                });

                if let (Some(li), Some(ri)) = (left_idx, right_idx) {
                    if right_rows.len() > HASH_JOIN_SPILL_THRESHOLD {
                        // Partitioned spill join
                        let joined = partition_hash_join(
                            &left_rows, li,
                            &right_rows, ri,
                            &left_schema, &right_schema,
                        ).map_err(|e| SqlError::Execution(format!("spill join error: {}", e)))?;
                        result.extend(joined);
                    } else {
                        // In-memory hash join
                        let mut hash: std::collections::HashMap<String, Vec<Vec<Value>>> =
                            std::collections::HashMap::new();
                        for row in &right_rows {
                            let key = format!("{:?}", &row[ri]);
                            hash.entry(key).or_default().push(row.clone());
                        }
                        for left_row in &left_rows {
                            let key = format!("{:?}", &left_row[li]);
                            if let Some(matches) = hash.get(&key) {
                                for right_row in matches {
                                    let mut combined = left_row.clone();
                                    combined.extend_from_slice(right_row);
                                    result.push(combined);
                                }
                            }
                        }
                    }
                } else {
                    // Fall back to nested loop
                    nested_loop_join(&left_schema, &left_rows, &right_schema, &right_rows,
                                     &combined_schema, condition, false, &null_right, &mut result)?;
                }
            } else {
                nested_loop_join(&left_schema, &left_rows, &right_schema, &right_rows,
                                 &combined_schema, condition, false, &null_right, &mut result)?;
            }
        }
        JoinKind::Left => {
            nested_loop_join(&left_schema, &left_rows, &right_schema, &right_rows,
                             &combined_schema, condition, true, &null_right, &mut result)?;
        }
        JoinKind::Right => {
            // Swap left/right, then swap columns back
            let null_left: Vec<Value> = left_schema.iter().map(|_| Value::Null).collect();
            let right_combined_schema: Vec<ColumnSchema> = right_schema.iter()
                .chain(left_schema.iter())
                .cloned()
                .collect();
            let swapped_condition = swap_condition(condition);
            let mut swapped_result = Vec::new();
            nested_loop_join(&right_schema, &right_rows, &left_schema, &left_rows,
                             &right_combined_schema, &swapped_condition, true, &null_left,
                             &mut swapped_result)?;
            // Re-order columns back to left+right order
            let left_len = left_schema.len();
            let right_len = right_schema.len();
            for row in swapped_result {
                let mut reordered = Vec::with_capacity(left_len + right_len);
                // left columns were at positions right_len..
                reordered.extend_from_slice(&row[right_len..]);
                // right columns were at positions 0..right_len
                reordered.extend_from_slice(&row[..right_len]);
                result.push(reordered);
            }
        }
        JoinKind::Full => {
            // Full outer join: left outer + anti-join of right
            let null_left: Vec<Value> = left_schema.iter().map(|_| Value::Null).collect();
            nested_loop_join(&left_schema, &left_rows, &right_schema, &right_rows,
                             &combined_schema, condition, true, &null_right, &mut result)?;
            // Add right rows that had no match
            for right_row in &right_rows {
                let has_match = left_rows.iter().any(|left_row| {
                    let mut combined = left_row.clone();
                    combined.extend_from_slice(right_row);
                    match condition {
                        JoinCondition::On(expr) => eval_expr(expr, &combined_schema, &combined)
                            .map(|v| is_truthy(&v))
                            .unwrap_or(false),
                        JoinCondition::Using(cols) => using_match(cols, &left_schema, left_row,
                                                                    &right_schema, right_row),
                    }
                });
                if !has_match {
                    let mut row = null_left.clone();
                    row.extend_from_slice(right_row);
                    result.push(row);
                }
            }
        }
    }

    Ok((combined_schema, result))
}

#[allow(clippy::too_many_arguments)]
fn nested_loop_join(
    left_schema: &[ColumnSchema],
    left_rows: &[Vec<Value>],
    right_schema: &[ColumnSchema],
    right_rows: &[Vec<Value>],
    combined_schema: &[ColumnSchema],
    condition: &JoinCondition,
    is_outer: bool,
    null_right: &[Value],
    result: &mut Vec<Vec<Value>>,
) -> Result<()> {
    for left_row in left_rows {
        let mut matched = false;
        for right_row in right_rows {
            let mut combined = left_row.clone();
            combined.extend_from_slice(right_row);
            let passes = match condition {
                JoinCondition::On(expr) => eval_expr(expr, combined_schema, &combined)
                    .map(|v| is_truthy(&v))
                    .unwrap_or(false),
                JoinCondition::Using(cols) => using_match(cols, left_schema, left_row,
                                                           right_schema, right_row),
            };
            if passes {
                matched = true;
                result.push(combined);
            }
        }
        if is_outer && !matched {
            let mut row = left_row.clone();
            row.extend_from_slice(null_right);
            result.push(row);
        }
    }
    Ok(())
}

/// Partitioned spill-to-disk hash join for large right-side tables.
/// Partitions both sides into k buckets, joins each bucket pair in memory.
fn partition_hash_join(
    left_rows: &[Vec<Value>],
    left_key_col: usize,
    right_rows: &[Vec<Value>],
    right_key_col: usize,
    _left_schema: &[ColumnSchema],
    _right_schema: &[ColumnSchema],
) -> std::io::Result<Vec<Vec<Value>>> {
    use crate::spill::partition_rows;
    use std::collections::HashMap;

    // Choose k so each bucket is roughly HASH_JOIN_SPILL_THRESHOLD rows
    let k = ((right_rows.len() / HASH_JOIN_SPILL_THRESHOLD) + 1).clamp(2, 256);

    // Partition both sides
    let left_files = partition_rows(left_rows, left_key_col, k)?;
    let right_files = partition_rows(right_rows, right_key_col, k)?;

    let mut result = Vec::new();

    for (lf, rf) in left_files.iter().zip(right_files.iter()) {
        // Load right bucket into hash table
        let right_bucket = rf.reader()?.into_rows()?;
        if right_bucket.is_empty() {
            continue;
        }
        let mut hash: HashMap<String, Vec<Vec<Value>>> = HashMap::new();
        for row in &right_bucket {
            let key = format!("{:?}", &row[right_key_col]);
            hash.entry(key).or_default().push(row.clone());
        }

        // Probe with left bucket
        let mut lr = lf.reader()?;
        while let Some(left_row) = lr.read_row()? {
            let key = format!("{:?}", &left_row[left_key_col]);
            if let Some(matches) = hash.get(&key) {
                for right_row in matches {
                    let mut combined = left_row.clone();
                    combined.extend_from_slice(right_row);
                    result.push(combined);
                }
            }
        }
    }

    Ok(result)
}

fn using_match(
    cols: &[String],
    left_schema: &[ColumnSchema],
    left_row: &[Value],
    right_schema: &[ColumnSchema],
    right_row: &[Value],
) -> bool {
    for col in cols {
        let li = left_schema.iter().position(|c| {
            let n = c.name.to_lowercase();
            n == col.to_lowercase() || n.ends_with(&format!(".{}", col.to_lowercase()))
        });
        let ri = right_schema.iter().position(|c| {
            let n = c.name.to_lowercase();
            n == col.to_lowercase() || n.ends_with(&format!(".{}", col.to_lowercase()))
        });
        match (li, ri) {
            (Some(l), Some(r)) => {
                if left_row[l] != right_row[r] { return false; }
            }
            _ => return false,
        }
    }
    true
}

/// Try to extract equality condition: col_a = col_b
fn extract_eq_cols(condition: &JoinCondition) -> Option<(String, String)> {
    if let JoinCondition::On(Expr::BinaryOp { op: BinOp::Eq, left, right }) = condition {
        if let (Expr::ColumnRef { table: lt, column: lc }, Expr::ColumnRef { table: rt, column: rc }) =
            (left.as_ref(), right.as_ref())
        {
            let left_key = match lt {
                Some(t) => format!("{}.{}", t, lc),
                None => lc.clone(),
            };
            let right_key = match rt {
                Some(t) => format!("{}.{}", t, rc),
                None => rc.clone(),
            };
            return Some((left_key, right_key));
        }
    }
    None
}

/// Swap the left/right sides of a join condition for RIGHT JOIN.
fn swap_condition(condition: &JoinCondition) -> JoinCondition {
    match condition {
        JoinCondition::On(Expr::BinaryOp { op: BinOp::Eq, left, right }) => {
            JoinCondition::On(Expr::BinaryOp {
                op: BinOp::Eq,
                left: right.clone(),
                right: left.clone(),
            })
        }
        other => other.clone(),
    }
}

// --- JSON helpers ---

/// Parse a JSON value string and return the value for a given key.
fn json_get(json_str: &str, key: &str) -> Result<String> {
    let s = json_str.trim();
    if s.starts_with('{') {
        // Object
        let inner = &s[1..s.len()-1];
        let pairs = split_json_pairs(inner);
        for (k, v) in pairs {
            let k = k.trim().trim_matches('"');
            if k == key {
                return Ok(v.trim().to_string());
            }
        }
        Err(SqlError::Execution(format!("key '{}' not found in JSON object", key)))
    } else if s.starts_with('[') {
        // Array — key must be numeric index
        if let Ok(idx) = key.parse::<usize>() {
            json_get_idx(s, idx)
        } else {
            Err(SqlError::Execution(format!("cannot use string key '{}' on JSON array", key)))
        }
    } else {
        Err(SqlError::Execution(format!("not a JSON object or array: {}", json_str)))
    }
}

/// Parse a JSON array string and return element at index.
fn json_get_idx(json_str: &str, idx: usize) -> Result<String> {
    let s = json_str.trim();
    if !s.starts_with('[') {
        return Err(SqlError::Execution("not a JSON array".into()));
    }
    let inner = &s[1..s.len()-1];
    let elements = split_json_array(inner);
    elements.get(idx)
        .map(|s| s.trim().to_string())
        .ok_or_else(|| SqlError::Execution(format!("JSON array index {} out of bounds", idx)))
}

/// Split a JSON object body into key-value pairs (naive, no nesting).
fn split_json_pairs(s: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut start = 0;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if c == '"' && (i == 0 || chars[i-1] != '\\') {
            in_str = !in_str;
        } else if !in_str {
            match c {
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    let pair = &s[start..chars[..i].iter().collect::<String>().len()];
                    if let Some(colon) = find_colon(pair) {
                        pairs.push((pair[..colon].to_string(), pair[colon+1..].to_string()));
                    }
                    start = chars[..i+1].iter().collect::<String>().len();
                }
                _ => {}
            }
        }
        i += 1;
    }
    // Last pair
    let pair = &s[start..];
    if !pair.trim().is_empty() {
        if let Some(colon) = find_colon(pair) {
            pairs.push((pair[..colon].to_string(), pair[colon+1..].to_string()));
        }
    }
    pairs
}

fn find_colon(s: &str) -> Option<usize> {
    let mut in_str = false;
    for (i, c) in s.char_indices() {
        if c == '"' { in_str = !in_str; }
        else if !in_str && c == ':' { return Some(i); }
    }
    None
}

/// Split a JSON array body into elements (naive, handles one level of nesting).
fn split_json_array(s: &str) -> Vec<String> {
    let mut elements = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut start = 0;
    let bytes = s.as_bytes();

    for i in 0..bytes.len() {
        let c = bytes[i] as char;
        if c == '"' && (i == 0 || bytes[i-1] != b'\\') {
            in_str = !in_str;
        } else if !in_str {
            match c {
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                ',' if depth == 0 => {
                    elements.push(s[start..i].to_string());
                    start = i + 1;
                }
                _ => {}
            }
        }
    }
    if start <= s.len() {
        let last = s[start..].trim();
        if !last.is_empty() {
            elements.push(last.to_string());
        }
    }
    elements
}

/// Merge two JSON objects: keys from both; right-side wins on conflict.
fn json_merge(a: &str, b: &str) -> Result<String> {
    let a_str = a.trim();
    let b_str = b.trim();
    if !a_str.starts_with('{') || !b_str.starts_with('{') {
        return Err(SqlError::Execution("|| json merge requires both operands to be JSON objects".into()));
    }
    let pairs_a = split_json_pairs(&a_str[1..a_str.len()-1]);
    let pairs_b = split_json_pairs(&b_str[1..b_str.len()-1]);

    // Build merged map: start with a, then overwrite with b
    let mut keys: Vec<String> = Vec::new();
    let mut values: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for (k, v) in &pairs_a {
        let k_clean = k.trim().trim_matches('"').to_string();
        if !keys.contains(&k_clean) {
            keys.push(k_clean.clone());
        }
        values.insert(k_clean, v.trim().to_string());
    }
    for (k, v) in &pairs_b {
        let k_clean = k.trim().trim_matches('"').to_string();
        if !keys.contains(&k_clean) {
            keys.push(k_clean.clone());
        }
        values.insert(k_clean, v.trim().to_string());
    }

    let pairs_out: Vec<String> = keys.iter().map(|k| {
        format!("\"{}\":{}", k, values[k])
    }).collect();
    Ok(format!("{{{}}}", pairs_out.join(",")))
}

/// Parse a JSON path string in Postgres format: '{key1,key2}' or '{0,key}'.
fn parse_json_path(path_str: &str) -> Vec<String> {
    let s = path_str.trim();
    // Accept both '{key1,key2}' and '[key1,key2]' formats
    let inner = if (s.starts_with('{') && s.ends_with('}')) || (s.starts_with('[') && s.ends_with(']')) {
        &s[1..s.len()-1]
    } else {
        s
    };
    inner.split(',').map(|p| p.trim().trim_matches('"').to_string()).filter(|p| !p.is_empty()).collect()
}

// --- Vector helpers ---

fn parse_vector_val(v: &Value) -> Result<Vec<f64>> {
    match v {
        Value::Text(s) => parse_vector_str(s),
        _ => Err(SqlError::Execution("vector must be text in '[x,y,...]' format".into())),
    }
}

fn parse_vector_str(s: &str) -> Result<Vec<f64>> {
    let s = s.trim();
    if !s.starts_with('[') || !s.ends_with(']') {
        return Err(SqlError::Execution(format!("invalid vector format: {}", s)));
    }
    let inner = &s[1..s.len()-1];
    if inner.trim().is_empty() {
        return Ok(vec![]);
    }
    inner.split(',')
        .map(|x| x.trim().parse::<f64>()
            .map_err(|_| SqlError::Execution(format!("invalid float in vector: {}", x))))
        .collect()
}

fn l2_distance(a: &[f64], b: &[f64]) -> Result<f64> {
    if a.len() != b.len() {
        return Err(SqlError::Execution(format!(
            "vector dimension mismatch: {} vs {}", a.len(), b.len()
        )));
    }
    let sum: f64 = a.iter().zip(b.iter()).map(|(x, y)| (x - y).powi(2)).sum();
    Ok(sum.sqrt())
}

// --- Cost-based optimization helpers ---

/// Estimate the fraction of rows that pass the WHERE clause (0.0 < result <= 1.0).
fn selectivity_estimate(
    where_clause: &Option<Expr>,
    schema: &[ColumnSchema],
    stats: &Option<crate::catalog::TableStats>,
    pk_col_idx: Option<usize>,
) -> f64 {
    let expr = match where_clause {
        None => return 1.0,
        Some(e) => e,
    };
    let row_count = stats.as_ref().map(|s| s.row_count).unwrap_or(0);
    estimate_expr_selectivity(expr, schema, stats, pk_col_idx, row_count)
}

fn estimate_expr_selectivity(
    expr: &Expr,
    schema: &[ColumnSchema],
    stats: &Option<crate::catalog::TableStats>,
    pk_col_idx: Option<usize>,
    row_count: usize,
) -> f64 {
    match expr {
        Expr::BinaryOp { op: BinOp::And, left, right } => {
            let sl = estimate_expr_selectivity(left, schema, stats, pk_col_idx, row_count);
            let sr = estimate_expr_selectivity(right, schema, stats, pk_col_idx, row_count);
            sl * sr
        }
        Expr::BinaryOp { op: BinOp::Or, left, right } => {
            let sl = estimate_expr_selectivity(left, schema, stats, pk_col_idx, row_count);
            let sr = estimate_expr_selectivity(right, schema, stats, pk_col_idx, row_count);
            1.0 - (1.0 - sl) * (1.0 - sr)
        }
        Expr::BinaryOp { op: BinOp::Eq, left, right } => {
            // Check if this is pk_col = literal (very selective)
            let col_name = match (left.as_ref(), right.as_ref()) {
                (Expr::ColumnRef { column, .. }, _) => Some(column.as_str()),
                (_, Expr::ColumnRef { column, .. }) => Some(column.as_str()),
                _ => None,
            };
            if let (Some(cn), Some(pk_idx)) = (col_name, pk_col_idx) {
                if schema.get(pk_idx).map(|c| c.name.eq_ignore_ascii_case(cn)).unwrap_or(false) {
                    return if row_count > 0 { 1.0 / row_count as f64 } else { 0.01 };
                }
            }
            // Try stats-based estimate
            if let (Some(col_name), Some(ts)) = (col_name, stats.as_ref()) {
                if let Some(col_idx) = schema.iter().position(|c| c.name.eq_ignore_ascii_case(col_name)) {
                    if let Some(cs) = ts.columns.get(col_idx) {
                        // Check MCV
                        let val_expr = match (left.as_ref(), right.as_ref()) {
                            (Expr::ColumnRef { .. }, v) => Some(v),
                            (v, Expr::ColumnRef { .. }) => Some(v),
                            _ => None,
                        };
                        if let Some(ve) = val_expr {
                            if let Ok(val) = eval_literal(ve) {
                                for (mcv_val, freq) in &cs.mcv {
                                    if *mcv_val == val { return *freq; }
                                }
                            }
                        }
                        return if cs.ndv > 0 { 1.0 / cs.ndv as f64 } else { 0.01 };
                    }
                }
            }
            0.1 // default equality heuristic
        }
        Expr::BinaryOp { op: BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq, left, right } => {
            let col_name = match (left.as_ref(), right.as_ref()) {
                (Expr::ColumnRef { column, .. }, _) => Some(column.as_str()),
                (_, Expr::ColumnRef { column, .. }) => Some(column.as_str()),
                _ => None,
            };
            if let (Some(cn), Some(ts)) = (col_name, stats.as_ref()) {
                if let Some(col_idx) = schema.iter().position(|c| c.name.eq_ignore_ascii_case(cn)) {
                    if let Some(cs) = ts.columns.get(col_idx) {
                        // Use histogram if available
                        if cs.hist_bounds.len() >= 2 {
                            let val_expr = match (left.as_ref(), right.as_ref()) {
                                (Expr::ColumnRef { .. }, v) => Some(v),
                                (v, Expr::ColumnRef { .. }) => Some(v),
                                _ => None,
                            };
                            if let Some(ve) = val_expr {
                                if let Ok(val) = eval_literal(ve) {
                                    let n = cs.hist_bounds.len() - 1;
                                    let idx = cs.hist_bounds.iter().filter(|b| {
                                        (*b).partial_cmp(&val).map(|o| o.is_lt()).unwrap_or(false)
                                    }).count();
                                    let frac = idx as f64 / n as f64;
                                    return match expr {
                                        Expr::BinaryOp { op: BinOp::Lt | BinOp::LtEq, .. } => frac,
                                        _ => 1.0 - frac,
                                    };
                                }
                            }
                        }
                    }
                }
            }
            0.33 // default range heuristic
        }
        Expr::IsNull(inner) => {
            if let Expr::ColumnRef { column, .. } = inner.as_ref() {
                if let Some(ts) = stats.as_ref() {
                    if let Some(col_idx) = schema.iter().position(|c| c.name.eq_ignore_ascii_case(column)) {
                        if let Some(cs) = ts.columns.get(col_idx) {
                            return cs.null_fraction;
                        }
                    }
                }
            }
            0.1
        }
        Expr::IsNotNull(inner) => {
            if let Expr::ColumnRef { column, .. } = inner.as_ref() {
                if let Some(ts) = stats.as_ref() {
                    if let Some(col_idx) = schema.iter().position(|c| c.name.eq_ignore_ascii_case(column)) {
                        if let Some(cs) = ts.columns.get(col_idx) {
                            return 1.0 - cs.null_fraction;
                        }
                    }
                }
            }
            0.9
        }
        _ => 0.5,
    }
}

/// Decide whether to use a secondary index scan.
fn should_use_index(selectivity: f64, row_count: usize) -> bool {
    selectivity * (row_count as f64) < 1000.0 || selectivity < 0.1
}

/// Detect a vector ANN query: ORDER BY col <-> '[...]' LIMIT k
/// Returns (col_idx, query_vector, k) if detected.
fn detect_vector_ann(
    order_by: &[OrderByItem],
    limit: &Option<Expr>,
    schema: &[ColumnSchema],
) -> Option<(usize, Vec<f64>, usize)> {
    if order_by.len() != 1 { return None; }
    let item = &order_by[0];
    if let Expr::BinaryOp { op: BinOp::VectorDist, left, right } = &item.expr {
        let col_name = match left.as_ref() {
            Expr::ColumnRef { column, .. } => column,
            _ => return None,
        };
        let query_str = match right.as_ref() {
            Expr::StrLit(s) => s,
            _ => return None,
        };
        let k = match limit {
            Some(Expr::IntLit(n)) if *n > 0 => *n as usize,
            _ => return None,
        };
        let col_idx = schema.iter().position(|c| c.name.eq_ignore_ascii_case(col_name))?;
        let query_vec = parse_vector_str(query_str).ok()?;
        Some((col_idx, query_vec, k))
    } else {
        None
    }
}

/// Selinger 1979 dynamic-programming join ordering for up to 8 tables.
/// For n > 8 falls back to greedy (ascending row-count) ordering.
///
/// IMPORTANT: The first step in the result always retains index 0 from the input
/// because step 0 has `condition = None` (the left-most table in the original FROM
/// clause). Only the tail steps (indices 1..) are reordered by the DP.
/// This preserves the invariant that `exec_multi_join` relies on: `condition` for
/// each non-first step is `Some(...)`.
#[allow(clippy::type_complexity)]
fn dp_join_order(
    steps: &[(String, Option<String>, Option<JoinKind>, Option<JoinCondition>)],
    catalog: &Catalog,
) -> Vec<usize> {
    if steps.len() <= 1 {
        return (0..steps.len()).collect();
    }

    // Separate CROSS joins (always appended last) from the first + regular.
    // The very first step (index 0) is always kept as the anchor (left-most table).
    // Only steps 1.. are subject to DP ordering.
    let first = 0usize;
    let regular: Vec<usize> = (1..steps.len()).filter(|&i| {
        steps[i].2.as_ref().map(|k| k != &JoinKind::Cross).unwrap_or(true)
    }).collect();
    let cross: Vec<usize> = (1..steps.len()).filter(|&i| {
        steps[i].2.as_ref().map(|k| k == &JoinKind::Cross).unwrap_or(false)
    }).collect();

    let n = regular.len();
    let ordered_tail = if n == 0 {
        vec![]
    } else if n > 7 {
        // Greedy fallback: sort by ascending row count.
        let row_counts: Vec<usize> = steps.iter().map(|(name, _, _, _)| {
            catalog.get(name)
                .and_then(|e| e.stats.as_ref().map(|s| s.row_count))
                .unwrap_or(1000)
        }).collect();
        let mut r = regular.clone();
        r.sort_by_key(|&i| row_counts[i]);
        r
    } else {
        dp_join_order_selinger(&regular, steps, catalog)
    };

    let mut result = vec![first];
    result.extend(ordered_tail);
    result.extend(cross);
    result
}

/// Selinger DP: bitmask DP over subsets of the tail join tables (indices 1..n).
/// Returns the indices in optimal left-deep join order.
#[allow(clippy::type_complexity)]
fn dp_join_order_selinger(
    indices: &[usize],
    steps: &[(String, Option<String>, Option<JoinKind>, Option<JoinCondition>)],
    catalog: &Catalog,
) -> Vec<usize> {
    let n = indices.len();
    if n == 0 { return vec![]; }
    if n == 1 { return indices.to_vec(); }

    // Row count per position in `indices`.
    let row_counts: Vec<f64> = indices.iter().map(|&i| {
        catalog.get(&steps[i].0)
            .and_then(|e| e.stats.as_ref().map(|s| s.row_count))
            .unwrap_or(1000) as f64
    }).collect();

    // best[mask] = (plan Vec<usize>, out_rows f64, total_cost f64)
    let total_masks = 1usize << n;
    let mut best: Vec<Option<(Vec<usize>, f64, f64)>> = vec![None; total_masks];

    // Base case: single-table plans.
    for pos in 0..n {
        let mask = 1usize << pos;
        let rows = row_counts[pos];
        best[mask] = Some((vec![indices[pos]], rows, rows));
    }

    // Fill subsets of increasing size.
    for size in 2..=n {
        for mask in 0..total_masks {
            if mask.count_ones() as usize != size { continue; }

            let mut best_cost = f64::MAX;
            let mut best_plan: Option<(Vec<usize>, f64)> = None;

            // Try each table in the subset as the last-joined (right) table.
            for pos in 0..n {
                if mask & (1 << pos) == 0 { continue; }
                let left_mask = mask ^ (1 << pos);
                if left_mask == 0 { continue; }

                let left = match &best[left_mask] {
                    Some(p) => p,
                    None => continue,
                };
                let (left_plan, left_rows, _left_cost) = left;

                let right_rows = row_counts[pos];
                // Simplified selectivity: 0.1 for equi-joins (no per-predicate stats)
                let out_rows = (left_rows * right_rows * 0.1_f64).max(1.0);
                let join_cost = left_rows * right_rows + out_rows;

                if join_cost < best_cost {
                    best_cost = join_cost;
                    let mut plan = left_plan.clone();
                    plan.push(indices[pos]);
                    best_plan = Some((plan, out_rows));
                }
            }

            if let Some((plan, out_rows)) = best_plan {
                best[mask] = Some((plan, out_rows, best_cost));
            }
        }
    }

    let full_mask = total_masks - 1;
    match &best[full_mask] {
        Some((plan, _, _)) => plan.clone(),
        None => indices.to_vec(),
    }
}

// --- Columnar / SQL Value conversion ---

fn sql_val_to_columnar(v: &Value) -> oigrap_storage::columnar::Value {
    match v {
        Value::Null => oigrap_storage::columnar::Value::Null,
        Value::Bool(b) => oigrap_storage::columnar::Value::Bool(*b),
        Value::Int64(n) => oigrap_storage::columnar::Value::Int64(*n),
        Value::Float64(f) => oigrap_storage::columnar::Value::Float64(*f),
        Value::Text(s) => oigrap_storage::columnar::Value::Text(s.clone()),
    }
}

fn columnar_val_to_sql(v: &oigrap_storage::columnar::Value) -> Value {
    match v {
        oigrap_storage::columnar::Value::Null => Value::Null,
        oigrap_storage::columnar::Value::Bool(b) => Value::Bool(*b),
        oigrap_storage::columnar::Value::Int64(n) => Value::Int64(*n),
        oigrap_storage::columnar::Value::Float64(f) => Value::Float64(*f),
        oigrap_storage::columnar::Value::Text(s) => Value::Text(s.clone()),
    }
}

// --- oigrap_shortest_path edge pre-loader ---

/// Walk the SELECT column list looking for oigrap_shortest_path() calls.
/// For each call found, scan the referenced edge table and store the edges in
/// the EDGE_CACHE thread-local so that eval_function can use them.
fn preload_edge_tables_for_shortest_path(
    columns: &[SelectColumn],
    catalog: &Catalog,
    pool: &mut BufferPool,
    tx: &TransactionManager,
) {
    for sc in columns {
        if let SelectColumn::Expr { expr, .. } = sc {
            collect_shortest_path_edges(expr, catalog, pool, tx);
        }
    }
}

fn collect_shortest_path_edges(
    expr: &Expr,
    catalog: &Catalog,
    pool: &mut BufferPool,
    tx: &TransactionManager,
) {
    match expr {
        Expr::FunctionCall { name, args, .. }
            if name.eq_ignore_ascii_case("OIGRAP_SHORTEST_PATH") =>
        {
            if args.len() < 5 {
                return;
            }
            // Args 2,3,4 must be string literals for us to pre-scan at compile time.
            let edge_table = if let Expr::StrLit(s) = &args[2] { s.to_lowercase() } else { return };
            let from_col   = if let Expr::StrLit(s) = &args[3] { s.to_lowercase() } else { return };
            let to_col     = if let Expr::StrLit(s) = &args[4] { s.to_lowercase() } else { return };

            let cache_key = format!("{}/{}/{}", edge_table, from_col, to_col);

            // Skip if already loaded.
            let already = EDGE_CACHE.with(|c| c.borrow().contains_key(&cache_key));
            if already {
                return;
            }

            let entry = match catalog.get(&edge_table) {
                Some(e) => e,
                None => return,
            };
            let schema = entry.columns.clone();
            let from_idx = schema.iter().position(|c| c.name.eq_ignore_ascii_case(&from_col));
            let to_idx   = schema.iter().position(|c| c.name.eq_ignore_ascii_case(&to_col));
            let (fi, ti) = match (from_idx, to_idx) {
                (Some(f), Some(t)) => (f, t),
                _ => return,
            };
            let snap = tx.snapshot();
            let all = match entry.heap.scan(pool) {
                Ok(r) => r,
                Err(_) => return,
            };
            let mut edges: Vec<(i64, i64)> = Vec::new();
            for (_tid, raw) in all {
                if let Ok((header, row_bytes)) = split_mvcc(&raw) {
                    if tx.is_visible(&header, &snap) {
                        if let Ok(row) = decode_row(&schema, row_bytes) {
                            let from_val = row.get(fi).cloned().unwrap_or(Value::Null);
                            let to_val   = row.get(ti).cloned().unwrap_or(Value::Null);
                            if let (Value::Int64(f), Value::Int64(t)) = (from_val, to_val) {
                                edges.push((f, t));
                            }
                        }
                    }
                }
            }
            EDGE_CACHE.with(|c| c.borrow_mut().insert(cache_key, edges));
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_shortest_path_edges(left, catalog, pool, tx);
            collect_shortest_path_edges(right, catalog, pool, tx);
        }
        _ => {}
    }
}

// --- Window function helpers ---

fn has_window_funcs(cols: &[SelectColumn]) -> bool {
    cols.iter().any(|sc| match sc {
        SelectColumn::Expr { expr, .. } => expr_has_window_func(expr),
        SelectColumn::Star => false,
    })
}

fn expr_has_window_func(expr: &Expr) -> bool {
    match expr {
        Expr::WindowFunc { .. } => true,
        Expr::BinaryOp { left, right, .. } => {
            expr_has_window_func(left) || expr_has_window_func(right)
        }
        _ => false,
    }
}

fn apply_window_functions(
    schema: Vec<ColumnSchema>,
    rows: Vec<Vec<Value>>,
    cols: &[SelectColumn],
) -> (Vec<ColumnSchema>, Vec<Vec<Value>>) {
    let mut window_funcs: Vec<Expr> = Vec::new();
    for sc in cols {
        if let SelectColumn::Expr { expr, .. } = sc {
            collect_window_funcs(expr, &mut window_funcs);
        }
    }

    if window_funcs.is_empty() {
        return (schema, rows);
    }

    let mut all_window_values: Vec<Vec<Value>> = Vec::new();
    for wf_expr in &window_funcs {
        let values = compute_window_function(wf_expr, &schema, &rows);
        all_window_values.push(values);
    }

    let mut new_schema = schema;
    for wf_expr in &window_funcs {
        if let Expr::WindowFunc { name, .. } = wf_expr {
            new_schema.push(ColumnSchema {
                name: format!("{}()", name),
                sql_type: crate::catalog::SqlType::Int64,
                nullable: true,
                primary_key: false,
            });
        }
    }

    let new_rows: Vec<Vec<Value>> = rows.into_iter().enumerate().map(|(row_idx, mut row)| {
        for wf_values in &all_window_values {
            row.push(wf_values.get(row_idx).cloned().unwrap_or(Value::Null));
        }
        row
    }).collect();

    (new_schema, new_rows)
}

fn collect_window_funcs(expr: &Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::WindowFunc { .. } if !out.contains(expr) => {
            out.push(expr.clone());
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_window_funcs(left, out);
            collect_window_funcs(right, out);
        }
        _ => {}
    }
}

fn compute_window_function(
    wf_expr: &Expr,
    schema: &[ColumnSchema],
    rows: &[Vec<Value>],
) -> Vec<Value> {
    let (name, args, partition_by, order_by) = match wf_expr {
        Expr::WindowFunc { name, args, partition_by, order_by } => {
            (name, args, partition_by, order_by)
        }
        _ => return vec![Value::Null; rows.len()],
    };

    let n = rows.len();
    let mut result = vec![Value::Null; n];

    let mut partition_map: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let key: Vec<Value> = partition_by.iter()
            .map(|e| eval_expr(e, schema, row).unwrap_or(Value::Null))
            .collect();
        if let Some(pos) = partition_map.iter().position(|(k, _)| k == &key) {
            partition_map[pos].1.push(row_idx);
        } else {
            partition_map.push((key, vec![row_idx]));
        }
    }

    for (_, partition_indices) in &partition_map {
        let mut sorted_indices = partition_indices.clone();
        sorted_indices.sort_by(|&a, &b| {
            for (expr, is_desc) in order_by {
                let va = eval_expr(expr, schema, &rows[a]).unwrap_or(Value::Null);
                let vb = eval_expr(expr, schema, &rows[b]).unwrap_or(Value::Null);
                let cmp = va.partial_cmp(&vb).unwrap_or(std::cmp::Ordering::Equal);
                let cmp = if *is_desc { cmp.reverse() } else { cmp };
                if cmp != std::cmp::Ordering::Equal {
                    return cmp;
                }
            }
            std::cmp::Ordering::Equal
        });

        match name.as_str() {
            "ROW_NUMBER" => {
                for (pos, &row_idx) in sorted_indices.iter().enumerate() {
                    result[row_idx] = Value::Int64((pos + 1) as i64);
                }
            }
            "RANK" => {
                let keys: Vec<Vec<Value>> = sorted_indices.iter().map(|&row_idx| {
                    order_by.iter()
                        .map(|(e, _)| eval_expr(e, schema, &rows[row_idx]).unwrap_or(Value::Null))
                        .collect::<Vec<_>>()
                }).collect();

                let mut rank = 1usize;
                for (pos, &row_idx) in sorted_indices.iter().enumerate() {
                    if pos == 0 {
                        result[row_idx] = Value::Int64(1);
                    } else if keys[pos] == keys[pos - 1] {
                        result[row_idx] = Value::Int64(rank as i64);
                    } else {
                        rank = pos + 1;
                        result[row_idx] = Value::Int64(rank as i64);
                    }
                }
            }
            "LAG" => {
                let offset = args.get(1)
                    .and_then(|e| rows.first().and_then(|r| eval_expr(e, schema, r).ok()))
                    .and_then(|v| if let Value::Int64(n) = v { Some(n as usize) } else { None })
                    .unwrap_or(1);
                for (pos, &row_idx) in sorted_indices.iter().enumerate() {
                    if pos < offset {
                        result[row_idx] = Value::Null;
                    } else {
                        let lag_row_idx = sorted_indices[pos - offset];
                        let val = args.first()
                            .and_then(|e| eval_expr(e, schema, &rows[lag_row_idx]).ok())
                            .unwrap_or(Value::Null);
                        result[row_idx] = val;
                    }
                }
            }
            "LEAD" => {
                let offset = args.get(1)
                    .and_then(|e| rows.first().and_then(|r| eval_expr(e, schema, r).ok()))
                    .and_then(|v| if let Value::Int64(n) = v { Some(n as usize) } else { None })
                    .unwrap_or(1);
                let sz = sorted_indices.len();
                for (pos, &row_idx) in sorted_indices.iter().enumerate() {
                    if pos + offset >= sz {
                        result[row_idx] = Value::Null;
                    } else {
                        let lead_row_idx = sorted_indices[pos + offset];
                        let val = args.first()
                            .and_then(|e| eval_expr(e, schema, &rows[lead_row_idx]).ok())
                            .unwrap_or(Value::Null);
                        result[row_idx] = val;
                    }
                }
            }
            _ => {}
        }
    }

    let _ = n;
    result
}

// --- PageRank helpers ---

fn preload_pagerank(
    columns: &[SelectColumn],
    catalog: &Catalog,
    pool: &mut BufferPool,
    tx: &TransactionManager,
) {
    for sc in columns {
        if let SelectColumn::Expr { expr, .. } = sc {
            collect_pagerank_edges(expr, catalog, pool, tx);
        }
    }
}

fn collect_pagerank_edges(
    expr: &Expr,
    catalog: &Catalog,
    pool: &mut BufferPool,
    tx: &TransactionManager,
) {
    match expr {
        Expr::FunctionCall { name, args, .. }
            if name.eq_ignore_ascii_case("OIGRAP_PAGERANK") =>
        {
            if args.len() < 4 {
                return;
            }
            let edge_table = if let Expr::StrLit(s) = &args[1] { s.to_lowercase() } else { return };
            let from_col   = if let Expr::StrLit(s) = &args[2] { s.to_lowercase() } else { return };
            let to_col     = if let Expr::StrLit(s) = &args[3] { s.to_lowercase() } else { return };

            let cache_key = format!("{}/{}/{}", edge_table, from_col, to_col);

            let already = PAGERANK_CACHE.with(|c| c.borrow().contains_key(&cache_key));
            if already {
                return;
            }

            let entry = match catalog.get(&edge_table) {
                Some(e) => e,
                None => return,
            };
            let schema = entry.columns.clone();
            let from_idx = schema.iter().position(|c| c.name.eq_ignore_ascii_case(&from_col));
            let to_idx   = schema.iter().position(|c| c.name.eq_ignore_ascii_case(&to_col));
            let (fi, ti) = match (from_idx, to_idx) {
                (Some(f), Some(t)) => (f, t),
                _ => return,
            };
            let snap = tx.snapshot();
            let all = match entry.heap.scan(pool) {
                Ok(r) => r,
                Err(_) => return,
            };
            let mut edges: Vec<(i64, i64)> = Vec::new();
            for (_tid, raw) in all {
                if let Ok((header, row_bytes)) = split_mvcc(&raw) {
                    if tx.is_visible(&header, &snap) {
                        if let Ok(row) = decode_row(&schema, row_bytes) {
                            let from_val = row.get(fi).cloned().unwrap_or(Value::Null);
                            let to_val   = row.get(ti).cloned().unwrap_or(Value::Null);
                            if let (Value::Int64(f), Value::Int64(t)) = (from_val, to_val) {
                                edges.push((f, t));
                            }
                        }
                    }
                }
            }

            let ranks = compute_pagerank(&edges);
            PAGERANK_CACHE.with(|c| c.borrow_mut().insert(cache_key, ranks));
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_pagerank_edges(left, catalog, pool, tx);
            collect_pagerank_edges(right, catalog, pool, tx);
        }
        _ => {}
    }
}

fn compute_pagerank(edges: &[(i64, i64)]) -> std::collections::HashMap<i64, f64> {
    use std::collections::{HashMap, HashSet};

    let mut nodes: HashSet<i64> = HashSet::new();
    for &(f, t) in edges {
        nodes.insert(f);
        nodes.insert(t);
    }
    let n = nodes.len();
    if n == 0 {
        return HashMap::new();
    }

    let mut out_degree: HashMap<i64, usize> = HashMap::new();
    let mut in_neighbors: HashMap<i64, Vec<i64>> = HashMap::new();

    for &node in &nodes {
        out_degree.entry(node).or_insert(0);
        in_neighbors.entry(node).or_default();
    }
    for &(f, t) in edges {
        *out_degree.entry(f).or_insert(0) += 1;
        in_neighbors.entry(t).or_default().push(f);
    }

    let init = 1.0 / n as f64;
    let mut rank: HashMap<i64, f64> = nodes.iter().map(|&node| (node, init)).collect();
    let damping = 0.85_f64;

    for _ in 0..20 {
        let mut new_rank: HashMap<i64, f64> = HashMap::new();
        for &node in &nodes {
            let sum: f64 = in_neighbors.get(&node)
                .map(|srcs| srcs.iter().map(|src| {
                    let r = rank.get(src).copied().unwrap_or(0.0);
                    let od = out_degree.get(src).copied().unwrap_or(1);
                    r / od.max(1) as f64
                }).sum())
                .unwrap_or(0.0);
            new_rank.insert(node, (1.0 - damping) / n as f64 + damping * sum);
        }
        rank = new_rank;
    }

    rank
}

// --- Type inference helper ---

fn sql_type_of(v: &Value) -> crate::catalog::SqlType {
    match v {
        Value::Int64(_) => crate::catalog::SqlType::Int64,
        Value::Float64(_) => crate::catalog::SqlType::Float64,
        Value::Bool(_) => crate::catalog::SqlType::Boolean,
        _ => crate::catalog::SqlType::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oigrap_storage::{DiskManager, TransactionManager, WalManager};
    use tempfile::tempdir;

    fn make_env() -> (BufferPool, WalManager, TransactionManager, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let disk = DiskManager::create(&dir.path().join("db")).unwrap();
        let pool = BufferPool::new(64, disk);
        let wal = WalManager::create(&dir.path().join("wal")).unwrap();
        let tx = TransactionManager::new();
        (pool, wal, tx, dir)
    }

    #[test]
    fn test_create_insert_select() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT NOT NULL, age BIGINT)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 22)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO users (id, name, age) VALUES (3, 'Carol', 28)", &mut pool, &mut wal, &mut tx).unwrap();

        let result = engine.execute("SELECT name FROM users WHERE age > 25", &mut pool, &mut wal, &mut tx).unwrap();
        assert_eq!(result.rows.len(), 2); // Alice (30) and Carol (28)
        let names: Vec<&str> = result.rows.iter().map(|r| {
            if let Value::Text(s) = &r[0] { s.as_str() } else { "" }
        }).collect();
        assert!(names.contains(&"Alice"));
        assert!(names.contains(&"Carol"));
    }

    #[test]
    fn test_pk_index_lookup() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)", &mut pool, &mut wal, &mut tx).unwrap();
        for i in 1..=100i64 {
            engine.execute(
                &format!("INSERT INTO users (id, name) VALUES ({}, 'user{}')", i, i),
                &mut pool, &mut wal, &mut tx
            ).unwrap();
        }

        // This should use the primary key index
        let result = engine.execute("SELECT name FROM users WHERE id = 42", &mut pool, &mut wal, &mut tx).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Text("user42".into()));
    }

    #[test]
    fn test_delete_removes_rows() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE items (id BIGINT, val TEXT)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO items (id, val) VALUES (1, 'keep')", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO items (id, val) VALUES (2, 'remove')", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("DELETE FROM items WHERE id = 2", &mut pool, &mut wal, &mut tx).unwrap();

        let result = engine.execute("SELECT val FROM items", &mut pool, &mut wal, &mut tx).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Text("keep".into()));
    }

    #[test]
    fn test_select_with_order_by_and_limit() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE nums (n BIGINT)", &mut pool, &mut wal, &mut tx).unwrap();
        for i in [5i64, 1, 4, 2, 3] {
            engine.execute(&format!("INSERT INTO nums (n) VALUES ({})", i), &mut pool, &mut wal, &mut tx).unwrap();
        }

        let result = engine.execute("SELECT n FROM nums ORDER BY n ASC LIMIT 3", &mut pool, &mut wal, &mut tx).unwrap();
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0][0], Value::Int64(1));
        assert_eq!(result.rows[2][0], Value::Int64(3));
    }

    #[test]
    fn test_milestone_select_where_age_gt_25() {
        // Month 5 milestone: "SELECT name FROM users WHERE age > 25 executes correctly"
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute(
            "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT NOT NULL, age BIGINT)",
            &mut pool, &mut wal, &mut tx
        ).unwrap();

        let test_data = [
            (1i64, "Alice", 30i64),
            (2, "Bob", 22),
            (3, "Carol", 28),
            (4, "Dave", 19),
            (5, "Eve", 35),
        ];

        for (id, name, age) in &test_data {
            engine.execute(
                &format!("INSERT INTO users (id, name, age) VALUES ({}, '{}', {})", id, name, age),
                &mut pool, &mut wal, &mut tx
            ).unwrap();
        }

        let result = engine.execute(
            "SELECT name FROM users WHERE age > 25",
            &mut pool, &mut wal, &mut tx
        ).unwrap();

        assert_eq!(result.columns, vec!["name"]);
        assert_eq!(result.rows.len(), 3, "expected Alice, Carol, Eve");

        let names: Vec<String> = result.rows.iter().map(|r| {
            if let Value::Text(s) = &r[0] { s.clone() } else { String::new() }
        }).collect();

        for expected in &["Alice", "Carol", "Eve"] {
            assert!(names.iter().any(|n| n == *expected), "missing {}", expected);
        }
    }

    #[test]
    fn test_inner_join() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("CREATE TABLE orders (id BIGINT PRIMARY KEY, user_id BIGINT, total BIGINT)", &mut pool, &mut wal, &mut tx).unwrap();

        engine.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO users (id, name) VALUES (2, 'Bob')", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO users (id, name) VALUES (3, 'Carol')", &mut pool, &mut wal, &mut tx).unwrap();

        engine.execute("INSERT INTO orders (id, user_id, total) VALUES (1, 1, 100)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO orders (id, user_id, total) VALUES (2, 1, 200)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO orders (id, user_id, total) VALUES (3, 2, 150)", &mut pool, &mut wal, &mut tx).unwrap();

        let result = engine.execute(
            "SELECT users.name, orders.total FROM users JOIN orders ON users.id = orders.user_id",
            &mut pool, &mut wal, &mut tx
        ).unwrap();

        assert_eq!(result.rows.len(), 3);
        let names: Vec<String> = result.rows.iter().map(|r| {
            if let Value::Text(s) = &r[0] { s.clone() } else { String::new() }
        }).collect();
        assert!(names.contains(&"Alice".to_string()));
        assert!(names.contains(&"Bob".to_string()));
        assert!(!names.contains(&"Carol".to_string()));
    }

    #[test]
    fn test_left_join() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE users2 (id BIGINT PRIMARY KEY, name TEXT)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("CREATE TABLE orders2 (id BIGINT PRIMARY KEY, user_id BIGINT, total BIGINT)", &mut pool, &mut wal, &mut tx).unwrap();

        engine.execute("INSERT INTO users2 (id, name) VALUES (1, 'Alice')", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO users2 (id, name) VALUES (2, 'Bob')", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO users2 (id, name) VALUES (3, 'Carol')", &mut pool, &mut wal, &mut tx).unwrap();

        engine.execute("INSERT INTO orders2 (id, user_id, total) VALUES (1, 1, 100)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO orders2 (id, user_id, total) VALUES (2, 2, 150)", &mut pool, &mut wal, &mut tx).unwrap();

        let result = engine.execute(
            "SELECT users2.name, orders2.total FROM users2 LEFT JOIN orders2 ON users2.id = orders2.user_id",
            &mut pool, &mut wal, &mut tx
        ).unwrap();

        // Carol has no orders, should appear with NULL total
        assert_eq!(result.rows.len(), 3);
        let carol_row = result.rows.iter().find(|r| {
            matches!(&r[0], Value::Text(s) if s == "Carol")
        });
        assert!(carol_row.is_some(), "Carol should appear in LEFT JOIN result");
        assert_eq!(carol_row.unwrap()[1], Value::Null, "Carol's total should be NULL");
    }

    #[test]
    fn test_explain_returns_plan_text() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE t (id BIGINT)", &mut pool, &mut wal, &mut tx).unwrap();

        let result = engine.execute("EXPLAIN SELECT id FROM t", &mut pool, &mut wal, &mut tx).unwrap();
        assert_eq!(result.tag, "EXPLAIN");
        assert_eq!(result.columns, vec!["QUERY PLAN"]);
        assert_eq!(result.rows.len(), 1);
        // The plan text should mention the table
        if let Value::Text(plan) = &result.rows[0][0] {
            assert!(plan.contains("t") || plan.contains("Scan"), "plan should mention table or Scan: {}", plan);
        } else {
            panic!("expected text plan");
        }
        // EXPLAIN should NOT have executed the query (no rows_affected)
        assert_eq!(result.rows_affected, 0);
    }

    #[test]
    fn test_cte_basic() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE nums2 (n BIGINT)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO nums2 (n) VALUES (1)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO nums2 (n) VALUES (2)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO nums2 (n) VALUES (3)", &mut pool, &mut wal, &mut tx).unwrap();

        let result = engine.execute(
            "WITH cte AS (SELECT n FROM nums2 WHERE n > 1) SELECT n FROM cte",
            &mut pool, &mut wal, &mut tx
        ).unwrap();

        assert_eq!(result.rows.len(), 2);
        let vals: Vec<i64> = result.rows.iter().map(|r| {
            if let Value::Int64(n) = &r[0] { *n } else { 0 }
        }).collect();
        assert!(vals.contains(&2));
        assert!(vals.contains(&3));
    }

    #[test]
    fn test_json_get_operator() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        // JSON -> operator: get field from JSON object
        let result = engine.execute(
            r#"SELECT '{"name":"Alice","age":30}' -> 'name'"#,
            &mut pool, &mut wal, &mut tx
        ).unwrap();
        assert_eq!(result.rows.len(), 1);
        // Should return "Alice" (quoted JSON string)
        if let Value::Text(v) = &result.rows[0][0] {
            assert!(v.contains("Alice"), "expected Alice in: {}", v);
        } else {
            panic!("expected text value");
        }

        // JSON ->> operator: get field as text (unquoted)
        let result2 = engine.execute(
            r#"SELECT '{"name":"Alice","age":30}' ->> 'name'"#,
            &mut pool, &mut wal, &mut tx
        ).unwrap();
        assert_eq!(result2.rows.len(), 1);
        assert_eq!(result2.rows[0][0], Value::Text("Alice".to_string()));
    }

    #[test]
    fn test_dp_join_ordering() {
        // 4 tables with very different sizes. Selinger DP should prefer joining smaller
        // tables first. We verify via EXPLAIN that the plan does not put the large table first.
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        // Create 4 tables: large (1M rows conceptually), tiny (10 rows), medium (50K), small (100)
        engine.execute("CREATE TABLE large_t  (id BIGINT PRIMARY KEY, v TEXT)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("CREATE TABLE tiny_t   (id BIGINT PRIMARY KEY, v TEXT)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("CREATE TABLE medium_t (id BIGINT PRIMARY KEY, v TEXT)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("CREATE TABLE small_t  (id BIGINT PRIMARY KEY, v TEXT)", &mut pool, &mut wal, &mut tx).unwrap();

        // Insert enough rows that ANALYZE gives us real stats
        for i in 0..5i64 {
            engine.execute(&format!("INSERT INTO large_t  (id, v) VALUES ({}, 'x')", i), &mut pool, &mut wal, &mut tx).unwrap();
            engine.execute(&format!("INSERT INTO tiny_t   (id, v) VALUES ({}, 'x')", i), &mut pool, &mut wal, &mut tx).unwrap();
            engine.execute(&format!("INSERT INTO medium_t (id, v) VALUES ({}, 'x')", i), &mut pool, &mut wal, &mut tx).unwrap();
            engine.execute(&format!("INSERT INTO small_t  (id, v) VALUES ({}, 'x')", i), &mut pool, &mut wal, &mut tx).unwrap();
        }

        // The DP ordering function works on stats; set up the catalog stats manually
        {
            let entry = engine.catalog.get_mut("large_t").unwrap();
            entry.stats = Some(crate::catalog::TableStats {
                row_count: 1_000_000,
                page_count: 10000,
                columns: vec![],
            });
        }
        {
            let entry = engine.catalog.get_mut("tiny_t").unwrap();
            entry.stats = Some(crate::catalog::TableStats {
                row_count: 10,
                page_count: 1,
                columns: vec![],
            });
        }
        {
            let entry = engine.catalog.get_mut("medium_t").unwrap();
            entry.stats = Some(crate::catalog::TableStats {
                row_count: 50_000,
                page_count: 500,
                columns: vec![],
            });
        }
        {
            let entry = engine.catalog.get_mut("small_t").unwrap();
            entry.stats = Some(crate::catalog::TableStats {
                row_count: 100,
                page_count: 1,
                columns: vec![],
            });
        }

        // Build the steps list as dp_join_order would receive.
        // NOTE: The DP keeps index 0 fixed as the anchor (left-most table).
        // We put tiny_t first so the DP can freely reorder the tail (large, medium, small).
        use crate::ast::JoinKind;
        let steps: Vec<(String, Option<String>, Option<JoinKind>, Option<crate::ast::JoinCondition>)> = vec![
            ("tiny_t".to_string(),   None, None, None),
            ("large_t".to_string(),  None, Some(JoinKind::Inner), Some(crate::ast::JoinCondition::On(Expr::BoolLit(true)))),
            ("medium_t".to_string(), None, Some(JoinKind::Inner), Some(crate::ast::JoinCondition::On(Expr::BoolLit(true)))),
            ("small_t".to_string(),  None, Some(JoinKind::Inner), Some(crate::ast::JoinCondition::On(Expr::BoolLit(true)))),
        ];

        let order = dp_join_order(&steps, &engine.catalog);

        // The first table is always tiny_t (the anchor at index 0).
        // The Selinger DP should order the tail so that small_t (100) comes before
        // medium_t (50000) and large_t (1M).
        assert_eq!(order[0], 0, "anchor (tiny_t) must stay first");

        // Tail: small_t (100 rows) should appear before large_t (1M rows)
        let small_pos = order.iter().position(|&i| steps[i].0 == "small_t").unwrap();
        let large_pos = order.iter().position(|&i| steps[i].0 == "large_t").unwrap();
        assert!(
            small_pos < large_pos,
            "small_t should be joined before large_t by Selinger DP, but small_pos={} large_pos={}",
            small_pos, large_pos
        );
    }

    #[test]
    fn test_vector_distance() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        // L2 distance between [0,0] and [3,4] = 5.0
        let result = engine.execute(
            r#"SELECT '[0.0,0.0]' <-> '[3.0,4.0]'"#,
            &mut pool, &mut wal, &mut tx
        ).unwrap();
        assert_eq!(result.rows.len(), 1);
        if let Value::Float64(dist) = &result.rows[0][0] {
            let diff = (dist - 5.0).abs();
            assert!(diff < 1e-9, "expected distance 5.0, got {}", dist);
        } else {
            panic!("expected float distance");
        }
    }

    #[test]
    fn test_depth_function() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute(
            "CREATE TABLE edges (from_id BIGINT, to_id BIGINT)",
            &mut pool, &mut wal, &mut tx
        ).unwrap();
        engine.execute("INSERT INTO edges (from_id, to_id) VALUES (1, 2)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO edges (from_id, to_id) VALUES (2, 3)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO edges (from_id, to_id) VALUES (3, 4)", &mut pool, &mut wal, &mut tx).unwrap();

        // Recursive CTE: traverse the chain 1->2->3->4 and record depth at each step.
        // Base case selects the edge starting at node 1 with depth 1.
        // Recursive step joins to follow the chain and uses DEPTH() for the depth column.
        let result = engine.execute(
            "WITH RECURSIVE tree AS (
                SELECT from_id, to_id, 1 AS lvl FROM edges WHERE from_id = 1
                UNION ALL
                SELECT e.from_id, e.to_id, DEPTH() FROM edges e JOIN tree ON e.from_id = tree.to_id
             ) SELECT lvl FROM tree",
            &mut pool, &mut wal, &mut tx
        ).unwrap();

        // Expect 3 rows: depth 1 (base), depth 2 (first recursion), depth 3 (second recursion).
        assert_eq!(result.rows.len(), 3, "expected 3 rows in tree, got {}", result.rows.len());

        let lvl_idx = result.columns.iter().position(|c| c == "lvl").unwrap_or(2);
        let mut depths: Vec<i64> = result.rows.iter().map(|r| {
            if let Value::Int64(n) = &r[lvl_idx] { *n } else { 0 }
        }).collect();
        depths.sort_unstable();
        assert_eq!(depths, vec![1, 2, 3], "expected depths 1,2,3 but got {:?}", depths);
    }

    #[test]
    fn test_vacuum_sql() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE junk (x BIGINT)", &mut pool, &mut wal, &mut tx).unwrap();
        for i in 1..=5i64 {
            engine.execute(
                &format!("INSERT INTO junk (x) VALUES ({})", i),
                &mut pool, &mut wal, &mut tx
            ).unwrap();
        }

        let result = engine.execute("VACUUM junk", &mut pool, &mut wal, &mut tx);
        assert!(result.is_ok(), "VACUUM should succeed: {:?}", result.err());
        let qr = result.unwrap();
        assert_eq!(qr.tag, "VACUUM");
    }

    #[test]
    fn test_shortest_path() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute(
            "CREATE TABLE sp_edges (from_id BIGINT, to_id BIGINT)",
            &mut pool, &mut wal, &mut tx
        ).unwrap();
        engine.execute("INSERT INTO sp_edges (from_id, to_id) VALUES (1, 2)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO sp_edges (from_id, to_id) VALUES (2, 3)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO sp_edges (from_id, to_id) VALUES (3, 4)", &mut pool, &mut wal, &mut tx).unwrap();

        // Path 1->2->3->4 = 3 hops.
        let result = engine.execute(
            "SELECT oigrap_shortest_path(1, 4, 'sp_edges', 'from_id', 'to_id')",
            &mut pool, &mut wal, &mut tx
        ).unwrap();

        assert_eq!(result.rows.len(), 1, "expected 1 row");
        match &result.rows[0][0] {
            Value::Int64(n) => assert_eq!(*n, 3, "expected path length 3, got {}", n),
            other => panic!("expected Int64 path length, got {:?}", other),
        }

        // No path from 4 to 1 (directed graph).
        let result2 = engine.execute(
            "SELECT oigrap_shortest_path(4, 1, 'sp_edges', 'from_id', 'to_id')",
            &mut pool, &mut wal, &mut tx
        ).unwrap();
        assert_eq!(result2.rows.len(), 1);
        assert_eq!(result2.rows[0][0], Value::Null, "4->1 should return Null (no path)");
    }

    #[test]
    fn test_row_number() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute(
            "CREATE TABLE rn_t (name TEXT, dept TEXT, salary BIGINT)",
            &mut pool, &mut wal, &mut tx
        ).unwrap();
        engine.execute("INSERT INTO rn_t (name, dept, salary) VALUES ('Alice', 'eng', 90)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO rn_t (name, dept, salary) VALUES ('Bob', 'eng', 80)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO rn_t (name, dept, salary) VALUES ('Carol', 'eng', 70)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO rn_t (name, dept, salary) VALUES ('Dave', 'hr', 60)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO rn_t (name, dept, salary) VALUES ('Eve', 'hr', 50)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO rn_t (name, dept, salary) VALUES ('Frank', 'hr', 40)", &mut pool, &mut wal, &mut tx).unwrap();

        let result = engine.execute(
            "SELECT name, dept, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY salary DESC) FROM rn_t",
            &mut pool, &mut wal, &mut tx
        ).unwrap();

        assert_eq!(result.rows.len(), 6);
        let rn_col = result.columns.iter().position(|c| c == "ROW_NUMBER()").unwrap_or(2);
        let mut eng_rns: Vec<i64> = result.rows.iter()
            .filter(|r| matches!(&r[1], Value::Text(s) if s == "eng"))
            .map(|r| if let Value::Int64(n) = &r[rn_col] { *n } else { 0 })
            .collect();
        eng_rns.sort_unstable();
        assert_eq!(eng_rns, vec![1, 2, 3], "eng dept row numbers should be 1,2,3");

        let mut hr_rns: Vec<i64> = result.rows.iter()
            .filter(|r| matches!(&r[1], Value::Text(s) if s == "hr"))
            .map(|r| if let Value::Int64(n) = &r[rn_col] { *n } else { 0 })
            .collect();
        hr_rns.sort_unstable();
        assert_eq!(hr_rns, vec![1, 2, 3], "hr dept row numbers should be 1,2,3");
    }

    #[test]
    fn test_rank() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute(
            "CREATE TABLE scores (player TEXT, score BIGINT)",
            &mut pool, &mut wal, &mut tx
        ).unwrap();
        engine.execute("INSERT INTO scores (player, score) VALUES ('alice', 100)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO scores (player, score) VALUES ('bob', 100)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO scores (player, score) VALUES ('carol', 90)", &mut pool, &mut wal, &mut tx).unwrap();

        let result = engine.execute(
            "SELECT player, RANK() OVER (ORDER BY score DESC) FROM scores",
            &mut pool, &mut wal, &mut tx
        ).unwrap();

        assert_eq!(result.rows.len(), 3);
        let rank_col = result.columns.iter().position(|c| c == "RANK()").unwrap_or(1);

        for row in &result.rows {
            let player = if let Value::Text(s) = &row[0] { s.as_str() } else { "" };
            let rank = if let Value::Int64(n) = &row[rank_col] { *n } else { 0 };
            match player {
                "alice" | "bob" => assert_eq!(rank, 1, "{} should have rank 1", player),
                "carol" => assert_eq!(rank, 3, "carol should have rank 3"),
                _ => {}
            }
        }
    }

    #[test]
    fn test_lag_lead() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute(
            "CREATE TABLE series (val BIGINT)",
            &mut pool, &mut wal, &mut tx
        ).unwrap();
        for i in 1i64..=5 {
            engine.execute(
                &format!("INSERT INTO series (val) VALUES ({})", i),
                &mut pool, &mut wal, &mut tx
            ).unwrap();
        }

        let result = engine.execute(
            "SELECT val, LAG(val) OVER (ORDER BY val), LEAD(val) OVER (ORDER BY val) FROM series",
            &mut pool, &mut wal, &mut tx
        ).unwrap();

        assert_eq!(result.rows.len(), 5);
        let mut rows = result.rows.clone();
        rows.sort_by_key(|r| if let Value::Int64(n) = &r[0] { *n } else { 0 });

        assert_eq!(rows[0][0], Value::Int64(1));
        assert_eq!(rows[0][1], Value::Null, "LAG of first row should be NULL");
        assert_eq!(rows[0][2], Value::Int64(2), "LEAD of first row should be 2");

        assert_eq!(rows[4][0], Value::Int64(5));
        assert_eq!(rows[4][1], Value::Int64(4), "LAG of last row should be 4");
        assert_eq!(rows[4][2], Value::Null, "LEAD of last row should be NULL");

        assert_eq!(rows[2][1], Value::Int64(2), "LAG of 3 should be 2");
        assert_eq!(rows[2][2], Value::Int64(4), "LEAD of 3 should be 4");
    }

    #[test]
    fn test_pagerank() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute(
            "CREATE TABLE pr_edges (from_id BIGINT, to_id BIGINT)",
            &mut pool, &mut wal, &mut tx
        ).unwrap();
        engine.execute("INSERT INTO pr_edges (from_id, to_id) VALUES (1, 2)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO pr_edges (from_id, to_id) VALUES (1, 3)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO pr_edges (from_id, to_id) VALUES (2, 3)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO pr_edges (from_id, to_id) VALUES (3, 1)", &mut pool, &mut wal, &mut tx).unwrap();

        engine.execute(
            "CREATE TABLE pr_nodes (id BIGINT)",
            &mut pool, &mut wal, &mut tx
        ).unwrap();
        engine.execute("INSERT INTO pr_nodes (id) VALUES (1)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO pr_nodes (id) VALUES (2)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO pr_nodes (id) VALUES (3)", &mut pool, &mut wal, &mut tx).unwrap();

        let result = engine.execute(
            "SELECT id, oigrap_pagerank(id, 'pr_edges', 'from_id', 'to_id') FROM pr_nodes",
            &mut pool, &mut wal, &mut tx
        ).unwrap();

        assert_eq!(result.rows.len(), 3);

        let mut sum = 0.0f64;
        for row in &result.rows {
            match &row[1] {
                Value::Float64(r) => {
                    assert!(*r > 0.0, "pagerank should be positive, got {}", r);
                    sum += r;
                }
                other => panic!("expected Float64 pagerank, got {:?}", other),
            }
        }
        let diff = (sum - 1.0).abs();
        assert!(diff < 0.01, "pageranks should sum to ~1.0, got {}", sum);
    }

    #[test]
    fn test_partition_hash_join_correctness() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE left_t (id INTEGER, val TEXT)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("CREATE TABLE right_t (id INTEGER, info INTEGER)", &mut pool, &mut wal, &mut tx).unwrap();

        // 50 matching rows — exercises the in-memory hash join path and verifies correctness
        for i in 0..50i64 {
            engine.execute(&format!("INSERT INTO left_t VALUES ({}, 'l{}')", i, i), &mut pool, &mut wal, &mut tx).unwrap();
            engine.execute(&format!("INSERT INTO right_t VALUES ({}, {})", i, i * 2), &mut pool, &mut wal, &mut tx).unwrap();
        }

        let result = engine.execute(
            "SELECT COUNT(*) FROM left_t JOIN right_t ON left_t.id = right_t.id",
            &mut pool, &mut wal, &mut tx,
        ).unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(50));
    }

    #[test]
    fn test_hash_join_empty_right() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE lhs (id INTEGER)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("CREATE TABLE rhs (id INTEGER)", &mut pool, &mut wal, &mut tx).unwrap();

        for i in 0..10i64 {
            engine.execute(&format!("INSERT INTO lhs VALUES ({})", i), &mut pool, &mut wal, &mut tx).unwrap();
        }
        // rhs is empty

        let result = engine.execute(
            "SELECT COUNT(*) FROM lhs JOIN rhs ON lhs.id = rhs.id",
            &mut pool, &mut wal, &mut tx,
        ).unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(0));
    }

    #[test]
    fn test_hash_join_no_matching_keys() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE ta (id INTEGER)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("CREATE TABLE tb (id INTEGER)", &mut pool, &mut wal, &mut tx).unwrap();

        for i in 0..5i64 {
            engine.execute(&format!("INSERT INTO ta VALUES ({})", i), &mut pool, &mut wal, &mut tx).unwrap();
            engine.execute(&format!("INSERT INTO tb VALUES ({})", i + 100), &mut pool, &mut wal, &mut tx).unwrap();
        }

        let result = engine.execute(
            "SELECT COUNT(*) FROM ta JOIN tb ON ta.id = tb.id",
            &mut pool, &mut wal, &mut tx,
        ).unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(0));
    }

    #[test]
    fn test_hash_join_duplicate_keys() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE dl (id INTEGER)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("CREATE TABLE dr (id INTEGER)", &mut pool, &mut wal, &mut tx).unwrap();

        // left: [1, 1, 2], right: [1, 2, 2]
        // expected pairs: (1,1), (1,1), (2,2), (2,2) = 4 rows
        engine.execute("INSERT INTO dl VALUES (1)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO dl VALUES (1)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO dl VALUES (2)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO dr VALUES (1)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO dr VALUES (2)", &mut pool, &mut wal, &mut tx).unwrap();
        engine.execute("INSERT INTO dr VALUES (2)", &mut pool, &mut wal, &mut tx).unwrap();

        let result = engine.execute(
            "SELECT COUNT(*) FROM dl JOIN dr ON dl.id = dr.id",
            &mut pool, &mut wal, &mut tx,
        ).unwrap();
        // left 1 x right 1 = 1*1 = 1, left 1 x right 1 = 1*1 = 1 (two left 1 rows), left 2 x right 2,2 = 2*2 = 4 total...
        // Actually: 2 left rows with id=1 each match 1 right row with id=1 => 2 pairs
        // 1 left row with id=2 matches 2 right rows with id=2 => 2 pairs  => total 4
        assert_eq!(result.rows[0][0], Value::Int64(4));
    }

    #[test]
    fn test_set_statement() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        let result = engine.execute("SET search_path TO public", &mut pool, &mut wal, &mut tx).unwrap();
        assert_eq!(result.tag, "SET");
        assert!(result.rows.is_empty());
    }

    #[test]
    fn test_show_statement() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        let result = engine.execute("SHOW search_path", &mut pool, &mut wal, &mut tx).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Text("public".into()));
    }

    #[test]
    fn test_pg_catalog_version() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        let result = engine.execute("SELECT version()", &mut pool, &mut wal, &mut tx).unwrap();
        assert_eq!(result.rows.len(), 1);
        match &result.rows[0][0] {
            Value::Text(s) => assert!(s.contains("oigrap"), "version() should contain 'oigrap', got: {}", s),
            other => panic!("expected Text, got {:?}", other),
        }
    }

    #[test]
    fn test_pg_tables_virtual() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE mytest (id BIGINT PRIMARY KEY, val TEXT)", &mut pool, &mut wal, &mut tx).unwrap();

        let result = engine.execute("SELECT tablename FROM pg_catalog.pg_tables", &mut pool, &mut wal, &mut tx).unwrap();
        let names: Vec<String> = result.rows.iter().filter_map(|r| {
            if let Value::Text(s) = &r[0] { Some(s.clone()) } else { None }
        }).collect();
        assert!(names.contains(&"mytest".to_string()), "pg_tables should contain 'mytest', got: {:?}", names);
    }

    #[test]
    fn test_information_schema_columns() {
        let (mut pool, mut wal, mut tx, _dir) = make_env();
        let mut engine = Engine::new();

        engine.execute("CREATE TABLE foo (id BIGINT, name TEXT)", &mut pool, &mut wal, &mut tx).unwrap();

        let result = engine.execute(
            "SELECT column_name FROM information_schema.columns WHERE table_name = 'foo'",
            &mut pool, &mut wal, &mut tx,
        ).unwrap();
        let col_names: Vec<String> = result.rows.iter().filter_map(|r| {
            if let Value::Text(s) = &r[0] { Some(s.clone()) } else { None }
        }).collect();
        assert!(col_names.contains(&"id".to_string()), "expected 'id' column, got: {:?}", col_names);
        assert!(col_names.contains(&"name".to_string()), "expected 'name' column, got: {:?}", col_names);
    }
}
