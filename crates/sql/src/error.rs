use std::fmt;

#[derive(Debug)]
pub struct ParseError {
    pub line: u32,
    pub col: u32,
    pub message: String,
}

impl ParseError {
    pub fn new(line: u32, col: u32, message: String) -> Self {
        ParseError { line, col, message }
    }

    pub fn at(message: String) -> Self {
        ParseError { line: 0, col: 0, message }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line > 0 {
            write!(f, "parse error at {}:{}: {}", self.line, self.col, self.message)
        } else {
            write!(f, "parse error: {}", self.message)
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug)]
pub enum SqlError {
    Parse(ParseError),
    Semantic(String),
    Execution(String),
    Storage(oigrap_storage::StorageError),
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqlError::Parse(e) => write!(f, "{}", e),
            SqlError::Semantic(m) => write!(f, "semantic error: {}", m),
            SqlError::Execution(m) => write!(f, "execution error: {}", m),
            SqlError::Storage(e) => write!(f, "storage error: {}", e),
        }
    }
}

impl std::error::Error for SqlError {}

impl From<ParseError> for SqlError {
    fn from(e: ParseError) -> Self {
        SqlError::Parse(e)
    }
}

impl From<oigrap_storage::StorageError> for SqlError {
    fn from(e: oigrap_storage::StorageError) -> Self {
        SqlError::Storage(e)
    }
}

pub type Result<T> = std::result::Result<T, SqlError>;
