use crate::ast::*;
use crate::error::{ParseError, Result, SqlError};
use crate::lexer::{Lexer, Token};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn parse(sql: &str) -> Result<Statement> {
        let tokens = Lexer::tokenize(sql)?;
        let mut p = Parser { tokens, pos: 0 };
        let stmt = p.parse_statement()?;
        // Consume optional trailing semicolon
        if p.peek() == &Token::Semicolon {
            p.advance();
        }
        Ok(stmt)
    }

    // --- Token stream helpers ---

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::EOF)
    }

    #[allow(dead_code)]
    fn peek2(&self) -> &Token {
        self.tokens.get(self.pos + 1).unwrap_or(&Token::EOF)
    }

    fn advance(&mut self) -> &Token {
        let tok = self.tokens.get(self.pos).unwrap_or(&Token::EOF);
        self.pos += 1;
        tok
    }

    fn expect(&mut self, expected: &Token) -> Result<()> {
        if self.peek() == expected {
            self.advance();
            Ok(())
        } else {
            Err(self.err(format!("expected {:?}, got {:?}", expected, self.peek())))
        }
    }

    fn expect_ident(&mut self) -> Result<String> {
        match self.advance().clone() {
            Token::Ident(s) | Token::QuotedIdent(s) => Ok(s),
            // Allow keywords as identifiers in table/column name positions
            other => {
                if let Some(s) = keyword_as_ident(&other) {
                    Ok(s)
                } else {
                    Err(self.err(format!("expected identifier, got {:?}", other)))
                }
            }
        }
    }

    fn err(&self, msg: String) -> SqlError {
        SqlError::Parse(ParseError::at(msg))
    }

    // --- Top-level ---

    fn parse_statement(&mut self) -> Result<Statement> {
        match self.peek().clone() {
            Token::Select | Token::With => Ok(Statement::Select(self.parse_select()?)),
            Token::Insert => self.parse_insert(),
            Token::Update => self.parse_update(),
            Token::Delete => self.parse_delete(),
            Token::Create => self.parse_create(),
            Token::Drop => self.parse_drop(),
            Token::Begin | Token::Transaction => { self.advance(); Ok(Statement::Begin) }
            Token::Commit => { self.advance(); Ok(Statement::Commit) }
            Token::Rollback => { self.advance(); Ok(Statement::Rollback) }
            Token::Explain => {
                self.advance();
                let inner = self.parse_statement()?;
                Ok(Statement::Explain(Box::new(inner)))
            }
            Token::Analyze => {
                self.advance();
                let table = self.expect_ident()?;
                Ok(Statement::Analyze(table))
            }
            Token::Vacuum => {
                self.advance();
                let table = self.expect_ident()?;
                Ok(Statement::Vacuum(table))
            }
            Token::Set => {
                self.advance();
                // Accept: SET [LOCAL|SESSION] name [=|TO] value
                // Skip LOCAL or SESSION keyword if present
                if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("LOCAL") || s.eq_ignore_ascii_case("SESSION")) {
                    self.advance();
                }
                let name = self.expect_ident().unwrap_or_default();
                // Accept = or TO
                if self.peek() == &Token::Eq
                    || matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("TO"))
                {
                    self.advance();
                }
                // Consume the value (could be ident, string literal, number, or DEFAULT)
                let value = match self.peek().clone() {
                    Token::StringLiteral(s) => { self.advance(); s }
                    Token::Ident(s) => { self.advance(); s }
                    Token::IntLiteral(n) => { self.advance(); n.to_string() }
                    _ => { self.advance(); String::new() }
                };
                Ok(Statement::SetVar { name, value })
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("SHOW") => {
                self.advance();
                let name = self.expect_ident().unwrap_or_else(|_| "search_path".to_string());
                Ok(Statement::ShowVar { name })
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("DISCARD") || s.eq_ignore_ascii_case("DEALLOCATE") => {
                // Consume remaining tokens — these are no-ops for compatibility
                while !matches!(self.peek(), Token::EOF | Token::Semicolon) {
                    self.advance();
                }
                Ok(Statement::SetVar { name: "discard".into(), value: String::new() })
            }
            other => Err(self.err(format!("unexpected token {:?}", other))),
        }
    }

    // --- SELECT ---

    fn parse_select(&mut self) -> Result<SelectStmt> {
        // Parse optional WITH clause
        let with_clauses = if self.peek() == &Token::With {
            self.advance();
            self.parse_cte_list()?
        } else {
            vec![]
        };

        self.expect(&Token::Select)?;

        let distinct = if self.peek() == &Token::Distinct {
            self.advance();
            true
        } else {
            if self.peek() == &Token::All { self.advance(); }
            false
        };

        let columns = self.parse_select_columns()?;

        let from = if self.peek() == &Token::From {
            self.advance();
            self.parse_table_refs()?
        } else {
            vec![]
        };

        let where_clause = if self.peek() == &Token::Where {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };

        let group_by = if self.peek() == &Token::Group {
            self.advance();
            self.expect(&Token::By)?;
            self.parse_expr_list()?
        } else {
            vec![]
        };

        let having = if self.peek() == &Token::Having {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };

        let order_by = if self.peek() == &Token::Order {
            self.advance();
            self.expect(&Token::By)?;
            self.parse_order_by()?
        } else {
            vec![]
        };

        let limit = if self.peek() == &Token::Limit {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };

        let offset = if self.peek() == &Token::Offset {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };

        Ok(SelectStmt { with_clauses, distinct, columns, from, where_clause, group_by, having, order_by, limit, offset })
    }

    fn parse_cte_list(&mut self) -> Result<Vec<CteClause>> {
        let mut ctes = vec![self.parse_cte()?];
        while self.peek() == &Token::Comma {
            self.advance();
            ctes.push(self.parse_cte()?);
        }
        Ok(ctes)
    }

    fn parse_cte(&mut self) -> Result<CteClause> {
        let recursive = if self.peek() == &Token::Recursive {
            self.advance();
            true
        } else {
            false
        };
        let name = self.expect_ident()?;
        self.expect(&Token::As)?;
        self.expect(&Token::LeftParen)?;
        let left = self.parse_select()?;
        // Detect UNION [ALL] inside CTE body
        let union_body = if self.peek() == &Token::Union {
            self.advance();
            let all = if self.peek() == &Token::All {
                self.advance();
                true
            } else {
                false
            };
            let right = self.parse_select()?;
            Some(crate::ast::UnionStmt {
                left: Box::new(left.clone()),
                all,
                right: Box::new(right),
            })
        } else {
            None
        };
        self.expect(&Token::RightParen)?;
        Ok(CteClause { name, recursive, query: Box::new(left), union_body })
    }

    fn parse_select_columns(&mut self) -> Result<Vec<SelectColumn>> {
        let mut cols = vec![self.parse_select_column()?];
        while self.peek() == &Token::Comma {
            self.advance();
            cols.push(self.parse_select_column()?);
        }
        Ok(cols)
    }

    fn parse_select_column(&mut self) -> Result<SelectColumn> {
        if self.peek() == &Token::Star {
            self.advance();
            return Ok(SelectColumn::Star);
        }
        let expr = self.parse_expr(0)?;
        let alias = if self.peek() == &Token::As {
            self.advance();
            Some(self.expect_ident()?)
        } else if matches!(self.peek(), Token::Ident(_)) {
            Some(self.expect_ident()?)
        } else {
            None
        };
        Ok(SelectColumn::Expr { expr, alias })
    }

    fn parse_table_refs(&mut self) -> Result<Vec<TableRef>> {
        let mut refs = vec![self.parse_table_ref()?];
        while self.peek() == &Token::Comma {
            self.advance();
            refs.push(self.parse_table_ref()?);
        }
        Ok(refs)
    }

    fn parse_table_ref(&mut self) -> Result<TableRef> {
        let first = self.expect_ident()?;
        // Handle schema-qualified table names: schema.table
        let name = if self.peek() == &Token::Dot {
            self.advance(); // consume '.'
            let table = self.expect_ident()?;
            format!("{}.{}", first, table)
        } else {
            first
        };
        let alias = if self.peek() == &Token::As {
            self.advance();
            Some(self.expect_ident()?)
        } else if matches!(self.peek(), Token::Ident(_) | Token::QuotedIdent(_)) {
            Some(self.expect_ident()?)
        } else {
            None
        };
        let join = self.parse_optional_join()?;
        Ok(TableRef { name, alias, join })
    }

    fn parse_optional_join(&mut self) -> Result<Option<Box<JoinClause>>> {
        let kind = match self.peek().clone() {
            Token::Join => { self.advance(); JoinKind::Inner }
            Token::Inner => {
                self.advance();
                self.expect(&Token::Join)?;
                JoinKind::Inner
            }
            Token::Left => {
                self.advance();
                if self.peek() == &Token::Outer { self.advance(); }
                self.expect(&Token::Join)?;
                JoinKind::Left
            }
            Token::Right => {
                self.advance();
                if self.peek() == &Token::Outer { self.advance(); }
                self.expect(&Token::Join)?;
                JoinKind::Right
            }
            Token::Full => {
                self.advance();
                if self.peek() == &Token::Outer { self.advance(); }
                self.expect(&Token::Join)?;
                JoinKind::Full
            }
            Token::Cross => {
                self.advance();
                self.expect(&Token::Join)?;
                JoinKind::Cross
            }
            _ => return Ok(None),
        };

        // Parse the right-side table name and optional alias
        let right_name = self.expect_ident()?;
        let right_alias = if self.peek() == &Token::As {
            self.advance();
            Some(self.expect_ident()?)
        } else if matches!(self.peek(), Token::Ident(_)) {
            Some(self.expect_ident()?)
        } else {
            None
        };

        // Parse ON or USING
        let condition = if self.peek() == &Token::On {
            self.advance();
            JoinCondition::On(self.parse_expr(0)?)
        } else if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("USING")) {
            self.advance();
            self.expect(&Token::LeftParen)?;
            let cols = self.parse_ident_list()?;
            self.expect(&Token::RightParen)?;
            JoinCondition::Using(cols)
        } else if kind == JoinKind::Cross {
            JoinCondition::On(Expr::BoolLit(true))
        } else {
            return Err(self.err("expected ON or USING after JOIN".into()));
        };

        // Recursively parse chained JOINs (a JOIN b ON ... JOIN c ON ...)
        let nested_join = self.parse_optional_join()?;
        let right = TableRef { name: right_name, alias: right_alias, join: nested_join };

        Ok(Some(Box::new(JoinClause { kind, right, condition })))
    }

    fn parse_order_by(&mut self) -> Result<Vec<OrderByItem>> {
        let mut items = vec![self.parse_order_item()?];
        while self.peek() == &Token::Comma {
            self.advance();
            items.push(self.parse_order_item()?);
        }
        Ok(items)
    }

    fn parse_order_item(&mut self) -> Result<OrderByItem> {
        let expr = self.parse_expr(0)?;
        let asc = if self.peek() == &Token::Desc {
            self.advance();
            false
        } else {
            if self.peek() == &Token::Asc { self.advance(); }
            true
        };
        Ok(OrderByItem { expr, asc, nulls_first: false })
    }

    // --- INSERT ---

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect(&Token::Insert)?;
        self.expect(&Token::Into)?;
        let table = self.expect_ident()?;

        let columns = if self.peek() == &Token::LeftParen {
            self.advance();
            let cols = self.parse_ident_list()?;
            self.expect(&Token::RightParen)?;
            cols
        } else {
            vec![]
        };

        self.expect(&Token::Values)?;
        let mut rows = vec![self.parse_value_row()?];
        while self.peek() == &Token::Comma {
            self.advance();
            rows.push(self.parse_value_row()?);
        }

        Ok(Statement::Insert(InsertStmt {
            table,
            columns,
            source: InsertSource::Values(rows),
        }))
    }

    fn parse_value_row(&mut self) -> Result<Vec<Expr>> {
        self.expect(&Token::LeftParen)?;
        let exprs = self.parse_expr_list()?;
        self.expect(&Token::RightParen)?;
        Ok(exprs)
    }

    // --- UPDATE ---

    fn parse_update(&mut self) -> Result<Statement> {
        self.expect(&Token::Update)?;
        let table = self.expect_ident()?;
        let alias = if self.peek() == &Token::As {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        self.expect(&Token::Set)?;
        let mut assignments = vec![self.parse_assignment()?];
        while self.peek() == &Token::Comma {
            self.advance();
            assignments.push(self.parse_assignment()?);
        }
        let where_clause = if self.peek() == &Token::Where {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        Ok(Statement::Update(UpdateStmt { table, alias, assignments, where_clause }))
    }

    fn parse_assignment(&mut self) -> Result<(String, Expr)> {
        let col = self.expect_ident()?;
        self.expect(&Token::Eq)?;
        let val = self.parse_expr(0)?;
        Ok((col, val))
    }

    // --- DELETE ---

    fn parse_delete(&mut self) -> Result<Statement> {
        self.expect(&Token::Delete)?;
        self.expect(&Token::From)?;
        let table = self.expect_ident()?;
        let alias = if self.peek() == &Token::As {
            self.advance();
            Some(self.expect_ident()?)
        } else {
            None
        };
        let where_clause = if self.peek() == &Token::Where {
            self.advance();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        Ok(Statement::Delete(DeleteStmt { table, alias, where_clause }))
    }

    // --- CREATE TABLE ---

    fn parse_create(&mut self) -> Result<Statement> {
        self.expect(&Token::Create)?;

        // CREATE VECTOR INDEX ON table (col)
        if matches!(self.peek(), Token::Vector) {
            self.advance(); // consume VECTOR
            // consume "INDEX"
            match self.peek().clone() {
                Token::Index => { self.advance(); }
                other => return Err(self.err(format!("expected INDEX after VECTOR, got {:?}", other))),
            }
            self.expect(&Token::On)?;
            let table = self.expect_ident()?;
            self.expect(&Token::LeftParen)?;
            let column = self.expect_ident()?;
            self.expect(&Token::RightParen)?;
            return Ok(Statement::CreateVectorIndex { table, column });
        }

        // CREATE GIN INDEX ON table (col)
        if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("GIN")) {
            self.advance(); // consume GIN
            match self.peek().clone() {
                Token::Index => { self.advance(); }
                other => return Err(self.err(format!("expected INDEX after GIN, got {:?}", other))),
            }
            self.expect(&Token::On)?;
            let table = self.expect_ident()?;
            self.expect(&Token::LeftParen)?;
            let column = self.expect_ident()?;
            self.expect(&Token::RightParen)?;
            return Ok(Statement::CreateGinIndex { table, column });
        }

        self.expect(&Token::Table)?;
        let if_not_exists = if self.peek() == &Token::If {
            self.advance();
            self.expect(&Token::Not)?;
            self.expect(&Token::Exists)?;
            true
        } else {
            false
        };
        let table = self.expect_ident()?;
        self.expect(&Token::LeftParen)?;

        let mut columns = Vec::new();
        let mut constraints = Vec::new();

        loop {
            if self.peek() == &Token::Primary {
                // Table-level PRIMARY KEY
                self.advance();
                self.expect(&Token::Key)?;
                self.expect(&Token::LeftParen)?;
                let cols = self.parse_ident_list()?;
                self.expect(&Token::RightParen)?;
                constraints.push(TableConstraint::PrimaryKey(cols));
            } else if self.peek() == &Token::Unique {
                self.advance();
                self.expect(&Token::LeftParen)?;
                let cols = self.parse_ident_list()?;
                self.expect(&Token::RightParen)?;
                constraints.push(TableConstraint::Unique(cols));
            } else {
                columns.push(self.parse_column_def()?);
            }

            if self.peek() == &Token::Comma {
                self.advance();
            } else {
                break;
            }
        }

        self.expect(&Token::RightParen)?;

        // Promote inline PRIMARY KEY to table constraint
        for col in &columns {
            if col.primary_key {
                constraints.push(TableConstraint::PrimaryKey(vec![col.name.clone()]));
            }
        }

        // Optional: WITH (STORAGE = COLUMNAR) or STORAGE COLUMNAR
        let storage = if self.peek() == &Token::Storage {
            self.advance();
            match self.peek().clone() {
                Token::Columnar => { self.advance(); StorageLayout::Columnar }
                Token::Row => { self.advance(); StorageLayout::Row }
                _ => StorageLayout::Row,
            }
        } else {
            StorageLayout::Row
        };

        Ok(Statement::CreateTable(CreateTableStmt {
            table,
            if_not_exists,
            columns,
            constraints,
            storage,
        }))
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef> {
        let name = self.expect_ident()?;
        let data_type = self.parse_data_type()?;
        let mut nullable = true;
        let mut primary_key = false;
        let mut unique = false;
        let mut default = None;

        loop {
            match self.peek().clone() {
                Token::Not => {
                    self.advance();
                    self.expect(&Token::Null)?;
                    nullable = false;
                }
                Token::Null => {
                    self.advance();
                    nullable = true;
                }
                Token::Primary => {
                    self.advance();
                    self.expect(&Token::Key)?;
                    primary_key = true;
                    nullable = false;
                }
                Token::Unique => {
                    self.advance();
                    unique = true;
                }
                Token::Default => {
                    self.advance();
                    default = Some(self.parse_expr(0)?);
                }
                _ => break,
            }
        }

        Ok(ColumnDef { name, data_type, nullable, default, primary_key, unique })
    }

    fn parse_data_type(&mut self) -> Result<DataType> {
        match self.advance().clone() {
            Token::Bool | Token::Boolean => Ok(DataType::Boolean),
            Token::Smallint => Ok(DataType::Int16),
            Token::Int | Token::Integer => Ok(DataType::Int32),
            Token::Bigint => Ok(DataType::Int64),
            Token::Float => Ok(DataType::Float32),
            Token::Double => {
                if self.peek() == &Token::Precision { self.advance(); }
                Ok(DataType::Float64)
            }
            Token::Text => Ok(DataType::Text),
            Token::Varchar => {
                let n = if self.peek() == &Token::LeftParen {
                    self.advance();
                    let n = self.parse_usize()?;
                    self.expect(&Token::RightParen)?;
                    n
                } else { 65535 };
                Ok(DataType::Varchar(n))
            }
            Token::Char => {
                let n = if self.peek() == &Token::LeftParen {
                    self.advance();
                    let n = self.parse_usize()?;
                    self.expect(&Token::RightParen)?;
                    n
                } else { 1 };
                Ok(DataType::Char(n))
            }
            Token::Bytea => Ok(DataType::Bytea),
            Token::Timestamp => Ok(DataType::Timestamp),
            Token::Date => Ok(DataType::Date),
            Token::Time => Ok(DataType::Time),
            Token::Json => Ok(DataType::Json),
            Token::Jsonb => Ok(DataType::Jsonb),
            Token::Vector => {
                self.expect(&Token::LeftParen)?;
                let n = self.parse_usize()?;
                self.expect(&Token::RightParen)?;
                Ok(DataType::Vector(n))
            }
            other => Err(self.err(format!("expected data type, got {:?}", other))),
        }
    }

    fn parse_usize(&mut self) -> Result<usize> {
        match self.advance().clone() {
            Token::IntLiteral(n) if n >= 0 => Ok(n as usize),
            other => Err(self.err(format!("expected positive integer, got {:?}", other))),
        }
    }

    // --- DROP TABLE ---

    fn parse_drop(&mut self) -> Result<Statement> {
        self.expect(&Token::Drop)?;
        self.expect(&Token::Table)?;
        let if_exists = if self.peek() == &Token::If {
            self.advance();
            self.expect(&Token::Exists)?;
            true
        } else {
            false
        };
        let table = self.expect_ident()?;
        Ok(Statement::DropTable(DropTableStmt { table, if_exists }))
    }

    // --- Expressions (Pratt parser) ---

    pub fn parse_expr(&mut self, min_bp: u8) -> Result<Expr> {
        let mut left = self.parse_prefix()?;

        loop {
            let (left_bp, right_bp) = infix_bp(self.peek());
            // left_bp == 0 means the token is not an infix operator; stop without consuming it.
            if left_bp == 0 || left_bp < min_bp {
                break;
            }
            let op_tok = self.advance().clone();
            let op = tok_to_binop(&op_tok);

            // Handle IS [NOT] NULL specially
            if op_tok == Token::Is {
                let negate = if self.peek() == &Token::Not {
                    self.advance();
                    true
                } else {
                    false
                };
                self.expect(&Token::Null)?;
                left = if negate {
                    Expr::IsNotNull(Box::new(left))
                } else {
                    Expr::IsNull(Box::new(left))
                };
                continue;
            }

            // Handle BETWEEN
            if op_tok == Token::Between {
                // Parse low at bp=21 so that AND (left_bp=20) terminates the low expression.
                let low = self.parse_expr(21)?;
                self.expect(&Token::And)?;
                let high = self.parse_expr(21)?;
                left = Expr::Between { expr: Box::new(left), low: Box::new(low), high: Box::new(high) };
                continue;
            }

            // Handle IN / NOT IN
            if op_tok == Token::In || (op_tok == Token::Not && self.peek() == &Token::In) {
                let negated = op_tok == Token::Not;
                if negated { self.advance(); } // consume IN
                self.expect(&Token::LeftParen)?;
                let list = self.parse_expr_list()?;
                self.expect(&Token::RightParen)?;
                left = if negated {
                    Expr::NotIn { expr: Box::new(left), list }
                } else {
                    Expr::In { expr: Box::new(left), list }
                };
                continue;
            }

            if let Some(bin_op) = op {
                let right = self.parse_expr(right_bp)?;
                left = Expr::BinaryOp { op: bin_op, left: Box::new(left), right: Box::new(right) };
            } else {
                break;
            }
        }

        Ok(left)
    }

    fn parse_prefix(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Token::IntLiteral(n) => { self.advance(); Ok(Expr::IntLit(n)) }
            Token::FloatLiteral(f) => { self.advance(); Ok(Expr::FloatLit(f)) }
            Token::StringLiteral(s) => { self.advance(); Ok(Expr::StrLit(s)) }
            Token::True => { self.advance(); Ok(Expr::BoolLit(true)) }
            Token::False => { self.advance(); Ok(Expr::BoolLit(false)) }
            Token::Null => { self.advance(); Ok(Expr::Null) }
            Token::Star => { self.advance(); Ok(Expr::Star) }
            Token::Minus => {
                self.advance();
                let e = self.parse_expr(70)?;
                Ok(Expr::UnaryOp { op: UnOp::Neg, expr: Box::new(e) })
            }
            Token::Not => {
                self.advance();
                let e = self.parse_expr(25)?;
                Ok(Expr::UnaryOp { op: UnOp::Not, expr: Box::new(e) })
            }
            Token::LeftParen => {
                self.advance();
                let e = self.parse_expr(0)?;
                self.expect(&Token::RightParen)?;
                Ok(e)
            }
            Token::Ident(_) | Token::QuotedIdent(_) => {
                let name = self.expect_ident()?;
                if self.peek() == &Token::Dot {
                    // table.column or schema.function()
                    self.advance();
                    let col = self.expect_ident()?;
                    if self.peek() == &Token::LeftParen {
                        // schema.function(...) — treat as function call with qualified name
                        self.advance();
                        let args = if self.peek() == &Token::RightParen {
                            vec![]
                        } else if self.peek() == &Token::Star {
                            self.advance();
                            vec![Expr::Star]
                        } else {
                            self.parse_expr_list()?
                        };
                        self.expect(&Token::RightParen)?;
                        let uname = format!("{}.{}", name.to_uppercase(), col.to_uppercase());
                        return Ok(Expr::FunctionCall { name: uname, args, distinct: false });
                    }
                    Ok(Expr::ColumnRef { table: Some(name), column: col })
                } else if self.peek() == &Token::LeftParen {
                    // function call
                    self.advance();
                    let args = if self.peek() == &Token::RightParen {
                        vec![]
                    } else if self.peek() == &Token::Star {
                        self.advance();
                        vec![Expr::Star]
                    } else {
                        self.parse_expr_list()?
                    };
                    self.expect(&Token::RightParen)?;
                    let uname = name.to_uppercase();
                    if matches!(&uname[..], "ROW_NUMBER" | "RANK" | "LAG" | "LEAD")
                        && matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("OVER")) {
                            self.advance();
                            self.expect(&Token::LeftParen)?;
                            let partition_by = if matches!(self.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("PARTITION")) {
                                self.advance();
                                self.expect(&Token::By)?;
                                self.parse_expr_list()?
                            } else {
                                vec![]
                            };
                            let order_by = if self.peek() == &Token::Order {
                                self.advance();
                                self.expect(&Token::By)?;
                                let items = self.parse_order_by()?;
                                items.into_iter().map(|item| (item.expr, !item.asc)).collect()
                            } else {
                                vec![]
                            };
                            self.expect(&Token::RightParen)?;
                            return Ok(Expr::WindowFunc { name: uname, args, partition_by, order_by });
                        }
                    Ok(Expr::FunctionCall { name: uname, args, distinct: false })
                } else {
                    Ok(Expr::ColumnRef { table: None, column: name })
                }
            }
            Token::Cast => {
                self.advance();
                self.expect(&Token::LeftParen)?;
                let e = self.parse_expr(0)?;
                self.expect(&Token::As)?;
                let to = self.parse_data_type()?;
                self.expect(&Token::RightParen)?;
                Ok(Expr::Cast { expr: Box::new(e), to })
            }
            other => Err(self.err(format!("unexpected token in expression: {:?}", other))),
        }
    }

    fn parse_expr_list(&mut self) -> Result<Vec<Expr>> {
        let mut exprs = vec![self.parse_expr(0)?];
        while self.peek() == &Token::Comma {
            self.advance();
            exprs.push(self.parse_expr(0)?);
        }
        Ok(exprs)
    }

    fn parse_ident_list(&mut self) -> Result<Vec<String>> {
        let mut ids = vec![self.expect_ident()?];
        while self.peek() == &Token::Comma {
            self.advance();
            ids.push(self.expect_ident()?);
        }
        Ok(ids)
    }
}

// Binding powers for Pratt parsing
fn infix_bp(tok: &Token) -> (u8, u8) {
    match tok {
        Token::Or => (10, 11),
        Token::And => (20, 21),
        Token::Not => (22, 22),   // NOT IN / NOT LIKE
        Token::Is => (30, 30),
        Token::Between => (30, 30),
        Token::In => (30, 30),
        Token::Like => (30, 31),
        Token::Contains => (30, 31),
        Token::QuestionMark => (30, 31),  // ? JSON key exists
        Token::HashArrow => (30, 31),     // #> JSON path
        Token::Eq | Token::NotEq => (40, 41),
        Token::Lt | Token::Gt | Token::LtEq | Token::GtEq => (50, 51),
        Token::Concat => (55, 56),
        Token::Plus | Token::Minus => (60, 61),
        Token::Arrow | Token::DoubleArrow => (60, 61),
        Token::VectorDist => (60, 61),
        Token::Star | Token::Slash | Token::Percent => (70, 71),
        Token::Caret => (80, 79), // right-associative
        _ => (0, 0),
    }
}

fn tok_to_binop(tok: &Token) -> Option<BinOp> {
    match tok {
        Token::Plus => Some(BinOp::Add),
        Token::Minus => Some(BinOp::Sub),
        Token::Star => Some(BinOp::Mul),
        Token::Slash => Some(BinOp::Div),
        Token::Percent => Some(BinOp::Mod),
        Token::Eq => Some(BinOp::Eq),
        Token::NotEq => Some(BinOp::NotEq),
        Token::Lt => Some(BinOp::Lt),
        Token::Gt => Some(BinOp::Gt),
        Token::LtEq => Some(BinOp::LtEq),
        Token::GtEq => Some(BinOp::GtEq),
        Token::And => Some(BinOp::And),
        Token::Or => Some(BinOp::Or),
        Token::Like => Some(BinOp::Like),
        Token::Concat => Some(BinOp::Concat),
        Token::Arrow => Some(BinOp::JsonGet),
        Token::DoubleArrow => Some(BinOp::JsonGetText),
        Token::Contains => Some(BinOp::JsonContains),
        Token::QuestionMark => Some(BinOp::JsonKeyExists),
        Token::HashArrow => Some(BinOp::JsonPath),
        Token::VectorDist => Some(BinOp::VectorDist),
        _ => None,
    }
}

fn keyword_as_ident(tok: &Token) -> Option<String> {
    match tok {
        Token::Text => Some("text".into()),
        Token::Row => Some("row".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_select() {
        let stmt = Parser::parse("SELECT id, name FROM users WHERE age > 25").unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.columns.len(), 2);
                assert!(s.where_clause.is_some());
            }
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn test_parse_insert_values() {
        let stmt = Parser::parse("INSERT INTO users (id, name) VALUES (1, 'Alice')").unwrap();
        match stmt {
            Statement::Insert(i) => {
                assert_eq!(i.table, "users");
                assert_eq!(i.columns, vec!["id", "name"]);
                match i.source {
                    InsertSource::Values(rows) => {
                        assert_eq!(rows.len(), 1);
                        assert_eq!(rows[0].len(), 2);
                    }
                    _ => panic!("expected Values"),
                }
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn test_parse_create_table() {
        let sql = "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT NOT NULL, age INT)";
        let stmt = Parser::parse(sql).unwrap();
        match stmt {
            Statement::CreateTable(ct) => {
                assert_eq!(ct.table, "users");
                assert_eq!(ct.columns.len(), 3);
                assert!(ct.constraints.iter().any(|c| matches!(c, TableConstraint::PrimaryKey(_))));
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn test_parse_delete() {
        let stmt = Parser::parse("DELETE FROM users WHERE id = 42").unwrap();
        assert!(matches!(stmt, Statement::Delete(_)));
    }

    #[test]
    fn test_parse_update() {
        let stmt = Parser::parse("UPDATE users SET name = 'Bob' WHERE id = 1").unwrap();
        assert!(matches!(stmt, Statement::Update(_)));
    }

    #[test]
    fn test_parse_expression_precedence() {
        // a + b * c should parse as a + (b * c)
        let stmt = Parser::parse("SELECT a + b * c FROM t").unwrap();
        match stmt {
            Statement::Select(s) => {
                match &s.columns[0] {
                    SelectColumn::Expr { expr: Expr::BinaryOp { op: BinOp::Add, right, .. }, .. } => {
                        assert!(matches!(right.as_ref(), Expr::BinaryOp { op: BinOp::Mul, .. }));
                    }
                    _ => panic!("wrong AST"),
                }
            }
            _ => panic!(),
        }
    }

    #[test]
    fn test_parse_is_null() {
        let stmt = Parser::parse("SELECT * FROM t WHERE x IS NULL").unwrap();
        match stmt {
            Statement::Select(s) => {
                assert!(matches!(s.where_clause, Some(Expr::IsNull(_))));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn test_parse_between() {
        let stmt = Parser::parse("SELECT * FROM t WHERE age BETWEEN 20 AND 30").unwrap();
        match stmt {
            Statement::Select(s) => {
                assert!(matches!(s.where_clause, Some(Expr::Between { .. })));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn test_parse_in_list() {
        let stmt = Parser::parse("SELECT * FROM t WHERE status IN (1, 2, 3)").unwrap();
        match stmt {
            Statement::Select(s) => {
                assert!(matches!(s.where_clause, Some(Expr::In { .. })));
            }
            _ => panic!(),
        }
    }
}
