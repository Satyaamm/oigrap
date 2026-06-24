# SQL Parser

The parser transforms a SQL string into a typed Abstract Syntax Tree (AST). The AST is the internal representation of a query that all subsequent phases (optimizer, executor) operate on.

The parser has no interaction with storage. It does not know whether tables exist or whether column types are correct. Type checking and semantic validation happen in a later phase (the logical planner).

---

## Architecture: two phases

```
SQL string
    |
    v
+----------+
|  Lexer   |  String -> []Token
+----------+
    |
    v
+----------+
|  Parser  |  []Token -> AST
+----------+
    |
    v
+----------+
| Rewriter |  AST -> AST (transformations)
+----------+
    |
    v
   AST
```

---

## 1. Lexer

The lexer (tokenizer) scans the input string character by character and produces a flat sequence of tokens. It handles:
- Whitespace and comments (discarded)
- Keywords (reserved words like SELECT, FROM, WHERE)
- Identifiers (table names, column names, aliases)
- String literals ('hello world')
- Numeric literals (42, 3.14, 1e-5)
- Operators (+, -, *, /, =, <, >, <>, <=, >=, <->, @>, ->>, ||, etc.)
- Punctuation (parentheses, commas, semicolons, dots)

### Token types

```rust
enum Token {
    // Keywords
    Select, From, Where, Join, On, As, Group, Order, By, Having,
    Insert, Into, Values, Update, Set, Delete,
    Create, Table, Drop, Alter, Index,
    And, Or, Not, In, Is, Null, Like, Between, Exists,
    Inner, Left, Right, Full, Cross, Outer, Natural,
    Asc, Desc, Limit, Offset, Distinct, All,
    True, False,
    With, Recursive, Union, Intersect, Except,
    Case, When, Then, Else, End,
    Cast, Interval, Timestamp, Date,
    Begin, Commit, Rollback, Transaction,
    Vacuum, Analyze, Explain,

    // Literals
    IntLiteral(i64),
    FloatLiteral(f64),
    StringLiteral(String),
    BitStringLiteral(String),

    // Identifiers
    Ident(String),
    QuotedIdent(String),  // "mixed case identifier"

    // Operators
    Plus, Minus, Star, Slash, Percent, Caret,
    Eq, NotEq, Lt, Gt, LtEq, GtEq,
    Arrow,          // ->
    DoubleArrow,    // ->>
    VectorDist,     // <->  (vector distance)
    Contains,       // @>
    ContainedBy,    // <@
    DoubleColon,    // ::  (cast)
    Concat,         // ||
    Dot,            // .

    // Punctuation
    LeftParen, RightParen,
    LeftBracket, RightBracket,
    Comma, Semicolon, Colon,
    Asterisk,

    // Special
    EOF,
}
```

### Lexer state machine

The lexer is a simple state machine. From the initial state, it dispatches on the first character:
- Letter or underscore: scan identifier or keyword
- Digit: scan numeric literal
- Single quote: scan string literal
- Double quote: scan quoted identifier
- Dash followed by dash: scan line comment
- Slash followed by star: scan block comment
- Other: scan single-character or multi-character operator

Keyword recognition: after scanning an identifier, check if it matches any keyword (case-insensitive). Use a perfect hash table or trie for O(1) keyword lookup.

```rust
struct Lexer<'a> {
    input: &'a str,
    pos: usize,
    line: u32,
    col: u32,
}

impl<'a> Lexer<'a> {
    fn next_token(&mut self) -> Result<Token>;
    fn peek_char(&self) -> Option<char>;
    fn advance(&mut self) -> char;
    fn scan_identifier(&mut self, first: char) -> Token;
    fn scan_string_literal(&mut self) -> Result<Token>;
    fn scan_numeric(&mut self, first: char) -> Result<Token>;
    fn scan_operator(&mut self, first: char) -> Result<Token>;
}
```

---

## 2. Parser

The parser is a recursive descent parser with Pratt parsing for expressions. Recursive descent means each grammar rule is a function. The function for SELECT calls the functions for the SELECT list, FROM clause, WHERE clause, etc.

### AST node types

```rust
enum Statement {
    Select(SelectStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    CreateTable(CreateTableStmt),
    CreateIndex(CreateIndexStmt),
    DropTable(DropTableStmt),
    DropIndex(DropIndexStmt),
    AlterTable(AlterTableStmt),
    Begin(BeginStmt),
    Commit,
    Rollback,
    Explain(ExplainStmt),
    Vacuum(VacuumStmt),
}

struct SelectStmt {
    ctes: Vec<CTE>,
    distinct: bool,
    columns: Vec<SelectColumn>,
    from: Vec<TableRef>,
    where_clause: Option<Expr>,
    group_by: Vec<Expr>,
    having: Option<Expr>,
    order_by: Vec<OrderByItem>,
    limit: Option<Expr>,
    offset: Option<Expr>,
    set_op: Option<(SetOp, Box<SelectStmt>)>,
}

struct InsertStmt {
    table: TableName,
    columns: Vec<String>,
    source: InsertSource,
    on_conflict: Option<OnConflict>,
    returning: Vec<SelectColumn>,
}

enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Select(SelectStmt),
}

struct UpdateStmt {
    table: TableName,
    alias: Option<String>,
    assignments: Vec<Assignment>,
    from: Vec<TableRef>,
    where_clause: Option<Expr>,
    returning: Vec<SelectColumn>,
}

struct DeleteStmt {
    table: TableName,
    alias: Option<String>,
    using: Vec<TableRef>,
    where_clause: Option<Expr>,
    returning: Vec<SelectColumn>,
}

struct CreateTableStmt {
    table: TableName,
    if_not_exists: bool,
    columns: Vec<ColumnDef>,
    constraints: Vec<TableConstraint>,
    storage_layout: StorageLayout,  // Row, Columnar, Auto
}

struct ColumnDef {
    name: String,
    data_type: DataType,
    nullable: bool,
    default: Option<Expr>,
    constraints: Vec<ColumnConstraint>,
}

enum DataType {
    Boolean,
    Int16, Int32, Int64,
    Float32, Float64, Decimal(u8, u8),
    Text, Varchar(usize), Char(usize),
    Bytea,
    Timestamp, TimestampTz, Date, Time,
    Uuid,
    Json, Jsonb,
    Vector(usize),       // vector(dimensions) -- our extension
    Array(Box<DataType>),
    UserDefined(String),
}
```

### Expression AST

Expressions appear in WHERE clauses, SELECT lists, JOIN conditions, and more. The expression AST must represent all possible expression types:

```rust
enum Expr {
    // Literals
    IntLit(i64),
    FloatLit(f64),
    StrLit(String),
    BoolLit(bool),
    Null,

    // References
    ColumnRef { table: Option<String>, column: String },
    Star,
    TableStar(String),

    // Operators
    BinaryOp { op: BinaryOp, left: Box<Expr>, right: Box<Expr> },
    UnaryOp  { op: UnaryOp,  expr: Box<Expr> },

    // Vector operator (extension)
    VectorDistance { left: Box<Expr>, right: Box<Expr> },   // <->

    // JSON operators (PostgreSQL-compatible)
    JsonGet     { expr: Box<Expr>, key: Box<Expr> },         // ->
    JsonGetText { expr: Box<Expr>, key: Box<Expr> },         // ->>
    JsonContains { left: Box<Expr>, right: Box<Expr> },      // @>

    // Predicates
    IsNull(Box<Expr>),
    IsNotNull(Box<Expr>),
    Between { expr: Box<Expr>, low: Box<Expr>, high: Box<Expr> },
    In { expr: Box<Expr>, list: Vec<Expr> },
    InSubquery { expr: Box<Expr>, subquery: Box<SelectStmt> },
    Exists(Box<SelectStmt>),
    Like { expr: Box<Expr>, pattern: Box<Expr>, escape: Option<Box<Expr>> },

    // Functions
    FunctionCall { name: String, args: Vec<Expr>, distinct: bool },
    Aggregate { func: AggFunc, arg: Option<Box<Expr>>, distinct: bool },

    // Conditional
    Case { base: Option<Box<Expr>>, when_clauses: Vec<(Expr, Expr)>, else_expr: Option<Box<Expr>> },

    // Subquery
    Subquery(Box<SelectStmt>),

    // Type cast
    Cast { expr: Box<Expr>, to: DataType },

    // Array
    ArrayLit(Vec<Expr>),
    ArrayIndex { expr: Box<Expr>, index: Box<Expr> },
}

enum BinaryOp {
    Add, Sub, Mul, Div, Mod, Pow,
    Eq, NotEq, Lt, Gt, LtEq, GtEq,
    And, Or,
    Like, NotLike, ILike, NotILike,
    Concat,
}

enum AggFunc {
    Count, Sum, Min, Max, Avg,
    StdDev, Variance,
    ArrayAgg, StringAgg,
    PercentileCont, PercentileDisc,
    FirstValue, LastValue, Lead, Lag,
    RowNumber, Rank, DenseRank,
}
```

### Pratt parser for expressions

Expressions have operator precedence: `a + b * c` means `a + (b * c)`, not `(a + b) * c`. Encoding this in recursive descent naively leads to deep recursion hierarchies.

Pratt parsing (top-down operator precedence) handles this elegantly. Each token type has two binding powers: prefix binding power (how tightly it binds to what follows it) and infix binding power (how tightly it binds to the expression on its left).

```rust
fn parse_expr(tokens: &mut TokenStream, min_bp: u8) -> Expr {
    let mut left = parse_prefix(tokens);

    loop {
        let op = tokens.peek();
        let (left_bp, right_bp) = infix_binding_power(op);
        if left_bp < min_bp { break; }
        tokens.advance();
        let right = parse_expr(tokens, right_bp);
        left = Expr::BinaryOp { op, left: Box::new(left), right: Box::new(right) };
    }

    left
}

fn infix_binding_power(op: &Token) -> (u8, u8) {
    match op {
        Token::Or                => (10, 11),
        Token::And               => (20, 21),
        Token::Eq | Token::NotEq => (30, 31),
        Token::Lt | Token::Gt
        | Token::LtEq | Token::GtEq => (40, 41),
        Token::Plus | Token::Minus   => (50, 51),
        Token::Star | Token::Slash   => (60, 61),
        Token::Caret             => (70, 69),  // right-associative
        Token::VectorDist        => (35, 36),  // <-> has its own precedence
        _                        => (0, 0),    // not an infix op
    }
}
```

### Key parsing functions

```rust
impl Parser {
    fn parse_statement(&mut self) -> Result<Statement>;
    fn parse_select(&mut self) -> Result<SelectStmt>;
    fn parse_from(&mut self) -> Result<Vec<TableRef>>;
    fn parse_join(&mut self, left: TableRef) -> Result<TableRef>;
    fn parse_where(&mut self) -> Result<Option<Expr>>;
    fn parse_group_by(&mut self) -> Result<Vec<Expr>>;
    fn parse_order_by(&mut self) -> Result<Vec<OrderByItem>>;
    fn parse_with(&mut self) -> Result<Vec<CTE>>;       // CTEs including RECURSIVE
    fn parse_create_table(&mut self) -> Result<CreateTableStmt>;
    fn parse_insert(&mut self) -> Result<InsertStmt>;
    fn parse_update(&mut self) -> Result<UpdateStmt>;
    fn parse_delete(&mut self) -> Result<DeleteStmt>;
    fn parse_expr(&mut self, min_bp: u8) -> Result<Expr>;
    fn parse_data_type(&mut self) -> Result<DataType>;
}
```

---

## 3. Query Rewriter

After parsing, the rewriter transforms the AST before planning. These are always-correct transformations — they do not change query semantics but simplify the AST for the optimizer.

### View expansion

When a query references a view, the view definition is substituted inline. The result is a larger AST with no view references. The optimizer never sees views.

### Subquery unnesting

Correlated subqueries (subqueries that reference columns from the outer query) are complex for the optimizer. Where possible, they are converted to joins.

```sql
-- Before: correlated subquery
SELECT u.name FROM users u
WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id);

-- After: rewritten as semi-join
SELECT u.name FROM users u
SEMI JOIN orders o ON o.user_id = u.id;
```

### Constant folding

Constant expressions are evaluated at parse time.

```sql
WHERE age > 20 + 5    -- rewritten to: WHERE age > 25
WHERE TRUE AND name = 'Alice'  -- rewritten to: WHERE name = 'Alice'
```

### NOT elimination

Double negation and NOT with comparison operators are simplified:

```sql
WHERE NOT (age > 30)    -- becomes: WHERE age <= 30
WHERE NOT (a AND b)     -- becomes: WHERE NOT a OR NOT b  (De Morgan)
```

### Implicit cast insertion

When a column of type INT is compared to a string literal, an implicit cast is inserted based on type rules. This happens during the planning phase after type checking, but the rewriter normalizes obvious cases.

---

## 4. Error reporting

Parse errors must include location information. Every token carries its line and column from the input. Error messages include:

```
ParseError at line 3, col 14:
  SELECT name FORM users WHERE id = 1;
              ^^^^
  Expected FROM, got identifier 'FORM'. Did you mean FROM?
```

Typo detection: when an unexpected identifier is encountered, compute Levenshtein distance to nearby keywords and suggest corrections if distance is 1-2.
