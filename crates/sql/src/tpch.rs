#[cfg(test)]
mod tests {
    use oigrap_storage::{BufferPool, DiskManager, TransactionManager, WalManager};
    use crate::{Engine, QueryResult, Value};

    fn make_env() -> (Engine, BufferPool, WalManager, TransactionManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let disk = DiskManager::create(&dir.path().join("db")).unwrap();
        let pool = BufferPool::new(256, disk);
        let wal = WalManager::create(&dir.path().join("wal")).unwrap();
        let tx = TransactionManager::new();
        let engine = Engine::new();
        (engine, pool, wal, tx, dir)
    }

    fn exec(
        engine: &mut Engine,
        pool: &mut BufferPool,
        wal: &mut WalManager,
        tx: &mut TransactionManager,
        sql: &str,
    ) -> QueryResult {
        engine.execute(sql, pool, wal, tx).expect(sql)
    }

    // TPC-H Q1 inspired: GROUP BY aggregate over a revenue table
    #[test]
    fn test_tpch_q1_revenue_by_category() {
        let (mut engine, mut pool, mut wal, mut tx, _dir) = make_env();

        exec(&mut engine, &mut pool, &mut wal, &mut tx,
            "CREATE TABLE lineitem (category TEXT, quantity INTEGER, price FLOAT, discount FLOAT)");

        // Insert test data: 3 categories
        let inserts = [
            ("A", 17, 24.0, 0.04),
            ("A", 21, 36.0, 0.09),
            ("A", 8,  15.0, 0.02),
            ("N", 15, 50.0, 0.00),
            ("N", 30, 20.0, 0.05),
            ("R", 5,  100.0, 0.10),
            ("R", 12, 80.0, 0.08),
        ];
        for (cat, qty, price, disc) in inserts {
            exec(&mut engine, &mut pool, &mut wal, &mut tx,
                &format!("INSERT INTO lineitem VALUES ('{}', {}, {}, {})", cat, qty, price, disc));
        }

        // Q1-style: aggregate by category
        let result = exec(&mut engine, &mut pool, &mut wal, &mut tx,
            "SELECT category, COUNT(*) FROM lineitem GROUP BY category ORDER BY category");

        assert_eq!(result.rows.len(), 3, "Expected 3 categories");

        let a_row = result.rows.iter().find(|r| r[0] == Value::Text("A".into())).expect("A row");
        assert_eq!(a_row[1], Value::Int64(3));

        let n_row = result.rows.iter().find(|r| r[0] == Value::Text("N".into())).expect("N row");
        assert_eq!(n_row[1], Value::Int64(2));

        let r_row = result.rows.iter().find(|r| r[0] == Value::Text("R".into())).expect("R row");
        assert_eq!(r_row[1], Value::Int64(2));
    }

    // TPC-H Q6 inspired: SUM filter (revenue calculation)
    #[test]
    fn test_tpch_q6_revenue_filter() {
        let (mut engine, mut pool, mut wal, mut tx, _dir) = make_env();

        exec(&mut engine, &mut pool, &mut wal, &mut tx,
            "CREATE TABLE lineitem2 (quantity INTEGER, price FLOAT, discount FLOAT)");

        let inserts: Vec<(i64, f64, f64)> = vec![
            (10, 100.0, 0.06),   // qualifies: discount in [0.05,0.07], qty < 24
            (20, 200.0, 0.05),   // qualifies
            (25, 300.0, 0.06),   // does NOT qualify: qty >= 24
            (5,  50.0,  0.03),   // does NOT qualify: discount < 0.05
            (8,  400.0, 0.07),   // qualifies
        ];
        for (qty, price, disc) in &inserts {
            exec(&mut engine, &mut pool, &mut wal, &mut tx,
                &format!("INSERT INTO lineitem2 VALUES ({}, {}, {})", qty, price, disc));
        }

        // Q6: count qualifying rows (discount >= 0.05 AND quantity < 24)
        // Qualifying: rows 0, 1, 4 => 3 rows
        let result = exec(&mut engine, &mut pool, &mut wal, &mut tx,
            "SELECT COUNT(*) FROM lineitem2 WHERE discount >= 0.05 AND quantity < 24");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(3),
            "Expected 3 qualifying rows");
    }

    // TPC-H Q3 inspired: multi-table join with filter
    #[test]
    fn test_tpch_q3_join_aggregate() {
        let (mut engine, mut pool, mut wal, mut tx, _dir) = make_env();

        exec(&mut engine, &mut pool, &mut wal, &mut tx,
            "CREATE TABLE orders (order_id INTEGER, customer_id INTEGER, status TEXT)");
        exec(&mut engine, &mut pool, &mut wal, &mut tx,
            "CREATE TABLE items (order_id INTEGER, amount FLOAT)");

        // Orders: 3 orders, statuses P/O/F
        exec(&mut engine, &mut pool, &mut wal, &mut tx, "INSERT INTO orders VALUES (1, 10, 'P')");
        exec(&mut engine, &mut pool, &mut wal, &mut tx, "INSERT INTO orders VALUES (2, 20, 'O')");
        exec(&mut engine, &mut pool, &mut wal, &mut tx, "INSERT INTO orders VALUES (3, 30, 'F')");

        // Items: multiple line items per order
        exec(&mut engine, &mut pool, &mut wal, &mut tx, "INSERT INTO items VALUES (1, 100.0)");
        exec(&mut engine, &mut pool, &mut wal, &mut tx, "INSERT INTO items VALUES (1, 200.0)");
        exec(&mut engine, &mut pool, &mut wal, &mut tx, "INSERT INTO items VALUES (2, 150.0)");
        exec(&mut engine, &mut pool, &mut wal, &mut tx, "INSERT INTO items VALUES (3, 50.0)");

        // Q3-style: join + filter + count
        let result = exec(&mut engine, &mut pool, &mut wal, &mut tx,
            "SELECT COUNT(*) FROM orders JOIN items ON orders.order_id = items.order_id WHERE orders.status = 'P'");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(2),
            "Order 1 (status P) has 2 items");
    }
}
