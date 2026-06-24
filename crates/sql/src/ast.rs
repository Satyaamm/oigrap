/// SQL Abstract Syntax Tree types.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Select(SelectStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    CreateTable(CreateTableStmt),
    DropTable(DropTableStmt),
    Begin,
    Commit,
    Rollback,
    Explain(Box<Statement>),
    Analyze(String),
    CreateVectorIndex { table: String, column: String },
    CreateGinIndex { table: String, column: String },
    /// VACUUM table_name — reclaim dead tuples from the named table.
    Vacuum(String),
    /// SET var = value (or SET var TO value) — session variable assignment, silently accepted.
    SetVar { name: String, value: String },
    /// SHOW var — return session variable value.
    ShowVar { name: String },
}

// ---- CTE ----

/// Represents SELECT ... UNION [ALL] SELECT ... inside a CTE body.
#[derive(Debug, Clone, PartialEq)]
pub struct UnionStmt {
    pub left: Box<SelectStmt>,
    pub all: bool,  // UNION ALL if true, UNION (dedup) if false
    pub right: Box<SelectStmt>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CteClause {
    pub name: String,
    pub recursive: bool,
    pub query: Box<SelectStmt>,
    /// For recursive CTEs: the union body splits the base case and recursive step.
    pub union_body: Option<UnionStmt>,
}

// ---- SELECT ----

#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub with_clauses: Vec<CteClause>,
    pub distinct: bool,
    pub columns: Vec<SelectColumn>,
    pub from: Vec<TableRef>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectColumn {
    Star,
    Expr { expr: Expr, alias: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
    pub join: Option<Box<JoinClause>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JoinClause {
    pub kind: JoinKind,
    pub right: TableRef,
    pub condition: JoinCondition,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JoinKind {
    Inner, Left, Right, Full, Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JoinCondition {
    On(Expr),
    Using(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    pub asc: bool,
    pub nulls_first: bool,
}

// ---- INSERT ----

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub table: String,
    pub columns: Vec<String>,
    pub source: InsertSource,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Select(Box<SelectStmt>),
}

// ---- UPDATE ----

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    pub table: String,
    pub alias: Option<String>,
    pub assignments: Vec<(String, Expr)>,
    pub where_clause: Option<Expr>,
}

// ---- DELETE ----

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub table: String,
    pub alias: Option<String>,
    pub where_clause: Option<Expr>,
}

// ---- CREATE TABLE ----

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStmt {
    pub table: String,
    pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<TableConstraint>,
    pub storage: StorageLayout,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub default: Option<Expr>,
    pub primary_key: bool,
    pub unique: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DataType {
    Boolean,
    Int16, Int32, Int64,
    Float32, Float64,
    Text,
    Varchar(usize),
    Char(usize),
    Bytea,
    Timestamp, Date, Time,
    Json, Jsonb,
    Vector(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableConstraint {
    PrimaryKey(Vec<String>),
    Unique(Vec<String>),
    Check(Expr),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum StorageLayout {
    #[default]
    Row,
    Columnar,
}

// ---- DROP TABLE ----

#[derive(Debug, Clone, PartialEq)]
pub struct DropTableStmt {
    pub table: String,
    pub if_exists: bool,
}

// ---- EXPRESSIONS ----

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    IntLit(i64),
    FloatLit(f64),
    StrLit(String),
    BoolLit(bool),
    Null,

    ColumnRef { table: Option<String>, column: String },
    Star,

    BinaryOp { op: BinOp, left: Box<Expr>, right: Box<Expr> },
    UnaryOp  { op: UnOp,  expr: Box<Expr> },

    IsNull(Box<Expr>),
    IsNotNull(Box<Expr>),
    Between { expr: Box<Expr>, low: Box<Expr>, high: Box<Expr> },
    In { expr: Box<Expr>, list: Vec<Expr> },
    NotIn { expr: Box<Expr>, list: Vec<Expr> },

    FunctionCall { name: String, args: Vec<Expr>, distinct: bool },

    WindowFunc {
        name: String,
        args: Vec<Expr>,
        partition_by: Vec<Expr>,
        order_by: Vec<(Expr, bool)>,
    },

    Cast { expr: Box<Expr>, to: DataType },
}

#[derive(Debug, Clone, PartialEq)]
pub enum BinOp {
    Add, Sub, Mul, Div, Mod,
    Eq, NotEq, Lt, Gt, LtEq, GtEq,
    And, Or,
    Like, NotLike,
    Concat,
    JsonGet,       // ->  returns JSON object/text
    JsonGetText,   // ->> returns text
    JsonContains,  // @>
    JsonKeyExists, // ?   key exists in object
    JsonMerge,     // ||  on JSON operands: merge two objects
    JsonPath,      // #>  path access '{key1,key2}'
    VectorDist,    // <->
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
}
