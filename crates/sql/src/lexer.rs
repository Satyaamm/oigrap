use crate::error::{ParseError, Result, SqlError};

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Keywords
    Select, From, Where, Join, On, As,
    Group, Order, By, Having,
    Insert, Into, Values,
    Update, Set,
    Delete,
    Create, Table, Drop, Alter, Index,
    And, Or, Not, In, Is, Null, Like, Between, Exists,
    Inner, Left, Right, Full, Cross, Outer,
    Asc, Desc, Limit, Offset, Distinct, All,
    True, False,
    With, Recursive, Union, Intersect, Except,
    Case, When, Then, Else, End,
    Cast, Primary, Key, Unique, References, Default, Check,
    If, Not_,   // NOT used for "NOT EXISTS" / "IF NOT EXISTS" disambiguation
    Begin, Commit, Rollback, Transaction,
    Vacuum, Analyze, Explain,
    Int, Bigint, Integer, Smallint, Float, Double, Precision,
    Bool, Boolean, Text, Varchar, Char, Bytea,
    Timestamp, Date, Time,
    Json, Jsonb,
    Vector, Null_,
    Storage, Columnar, Row,

    // Literals
    IntLiteral(i64),
    FloatLiteral(f64),
    StringLiteral(String),

    // Identifiers
    Ident(String),
    QuotedIdent(String),

    // Operators
    Plus, Minus, Star, Slash, Percent, Caret,
    Eq, NotEq, Lt, Gt, LtEq, GtEq,
    Arrow,           // ->
    DoubleArrow,     // ->>
    VectorDist,      // <->
    Contains,        // @>
    ContainedBy,     // <@
    DoubleColon,     // ::
    Concat,          // ||
    QuestionMark,    // ?  (JSON key exists)
    HashArrow,       // #> (JSON path access)

    // Punctuation
    Dot, Comma, Semicolon, Colon,
    LeftParen, RightParen,
    LeftBracket, RightBracket,

    EOF,
}

pub struct Lexer<'a> {
    input: &'a [u8],
    pos: usize,
    pub line: u32,
    pub col: u32,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Lexer { input: input.as_bytes(), pos: 0, line: 1, col: 1 }
    }

    pub fn tokenize(input: &'a str) -> Result<Vec<Token>> {
        let mut lex = Self::new(input);
        let mut tokens = Vec::new();
        loop {
            let tok = lex.next_token()?;
            let is_eof = tok == Token::EOF;
            tokens.push(tok);
            if is_eof {
                break;
            }
        }
        Ok(tokens)
    }

    pub fn next_token(&mut self) -> Result<Token> {
        self.skip_whitespace_and_comments();

        if self.pos >= self.input.len() {
            return Ok(Token::EOF);
        }

        let ch = self.input[self.pos] as char;

        match ch {
            'a'..='z' | 'A'..='Z' | '_' => Ok(self.scan_ident()),
            '0'..='9' => self.scan_number(),
            '\'' => self.scan_string(),
            '"' => self.scan_quoted_ident(),
            _ => self.scan_operator(),
        }
    }

    // --- Private helpers ---

    fn peek(&self) -> Option<char> {
        self.input.get(self.pos).map(|&b| b as char)
    }

    fn peek2(&self) -> Option<char> {
        self.input.get(self.pos + 1).map(|&b| b as char)
    }

    fn advance(&mut self) -> char {
        let ch = self.input[self.pos] as char;
        self.pos += 1;
        if ch == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        ch
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            // Skip whitespace
            while self.pos < self.input.len()
                && (self.input[self.pos] as char).is_ascii_whitespace()
            {
                self.advance();
            }
            // Line comment --
            if self.pos + 1 < self.input.len()
                && self.input[self.pos] == b'-'
                && self.input[self.pos + 1] == b'-'
            {
                while self.pos < self.input.len() && self.input[self.pos] != b'\n' {
                    self.pos += 1;
                }
                continue;
            }
            // Block comment /* ... */
            if self.pos + 1 < self.input.len()
                && self.input[self.pos] == b'/'
                && self.input[self.pos + 1] == b'*'
            {
                self.pos += 2;
                while self.pos + 1 < self.input.len()
                    && !(self.input[self.pos] == b'*' && self.input[self.pos + 1] == b'/')
                {
                    self.pos += 1;
                }
                self.pos += 2; // consume */
                continue;
            }
            break;
        }
    }

    fn scan_ident(&mut self) -> Token {
        let start = self.pos;
        while self.pos < self.input.len() {
            let c = self.input[self.pos] as char;
            if c.is_alphanumeric() || c == '_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let word = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
        keyword_or_ident(word)
    }

    fn scan_number(&mut self) -> Result<Token> {
        let start = self.pos;
        let mut is_float = false;
        while self.pos < self.input.len() && (self.input[self.pos] as char).is_ascii_digit() {
            self.pos += 1;
        }
        if self.pos < self.input.len() && self.input[self.pos] == b'.' {
            is_float = true;
            self.pos += 1;
            while self.pos < self.input.len() && (self.input[self.pos] as char).is_ascii_digit() {
                self.pos += 1;
            }
        }
        if self.pos < self.input.len()
            && (self.input[self.pos] == b'e' || self.input[self.pos] == b'E')
        {
            is_float = true;
            self.pos += 1;
            if self.pos < self.input.len()
                && (self.input[self.pos] == b'+' || self.input[self.pos] == b'-')
            {
                self.pos += 1;
            }
            while self.pos < self.input.len() && (self.input[self.pos] as char).is_ascii_digit() {
                self.pos += 1;
            }
        }
        let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
        if is_float {
            s.parse::<f64>()
                .map(Token::FloatLiteral)
                .map_err(|_| SqlError::Parse(ParseError::new(self.line, self.col, format!("invalid float: {}", s))))
        } else {
            s.parse::<i64>()
                .map(Token::IntLiteral)
                .map_err(|_| SqlError::Parse(ParseError::new(self.line, self.col, format!("invalid integer: {}", s))))
        }
    }

    fn scan_string(&mut self) -> Result<Token> {
        self.pos += 1; // consume opening '
        let mut s = String::new();
        loop {
            if self.pos >= self.input.len() {
                return Err(SqlError::Parse(ParseError::new(self.line, self.col, "unterminated string".into())));
            }
            let ch = self.advance();
            if ch == '\'' {
                // SQL escape: '' means a literal single quote
                if self.peek() == Some('\'') {
                    self.advance();
                    s.push('\'');
                } else {
                    break;
                }
            } else {
                s.push(ch);
            }
        }
        Ok(Token::StringLiteral(s))
    }

    fn scan_quoted_ident(&mut self) -> Result<Token> {
        self.pos += 1; // consume "
        let mut s = String::new();
        loop {
            if self.pos >= self.input.len() {
                return Err(SqlError::Parse(ParseError::new(self.line, self.col, "unterminated quoted identifier".into())));
            }
            let ch = self.advance();
            if ch == '"' {
                break;
            }
            s.push(ch);
        }
        Ok(Token::QuotedIdent(s))
    }

    fn scan_operator(&mut self) -> Result<Token> {
        let ch = self.advance();
        match ch {
            '+' => Ok(Token::Plus),
            '*' => Ok(Token::Star),
            '%' => Ok(Token::Percent),
            '^' => Ok(Token::Caret),
            ',' => Ok(Token::Comma),
            ';' => Ok(Token::Semicolon),
            '(' => Ok(Token::LeftParen),
            ')' => Ok(Token::RightParen),
            '[' => Ok(Token::LeftBracket),
            ']' => Ok(Token::RightBracket),
            '.' => Ok(Token::Dot),
            '=' => Ok(Token::Eq),
            '!' => {
                if self.peek() == Some('=') {
                    self.advance();
                    Ok(Token::NotEq)
                } else {
                    Err(SqlError::Parse(ParseError::new(self.line, self.col, "expected '=' after '!'".into())))
                }
            }
            '<' => match self.peek() {
                Some('=') => { self.advance(); Ok(Token::LtEq) }
                Some('>') => { self.advance(); Ok(Token::NotEq) }
                Some('-') if self.peek2() == Some('>') => {
                    self.advance(); self.advance();
                    Ok(Token::VectorDist)
                }
                Some('@') => { self.advance(); Ok(Token::ContainedBy) }
                _ => Ok(Token::Lt),
            },
            '>' => {
                if self.peek() == Some('=') { self.advance(); Ok(Token::GtEq) } else { Ok(Token::Gt) }
            }
            '-' => match self.peek() {
                Some('>') if self.peek2() == Some('>') => {
                    self.advance(); self.advance();
                    Ok(Token::DoubleArrow)
                }
                Some('>') => { self.advance(); Ok(Token::Arrow) }
                _ => Ok(Token::Minus),
            },
            '/' => Ok(Token::Slash),
            ':' => {
                if self.peek() == Some(':') { self.advance(); Ok(Token::DoubleColon) } else { Ok(Token::Colon) }
            }
            '|' => {
                if self.peek() == Some('|') { self.advance(); Ok(Token::Concat) } else {
                    Err(SqlError::Parse(ParseError::new(self.line, self.col, "unexpected '|'".to_string())))
                }
            }
            '@' => {
                if self.peek() == Some('>') { self.advance(); Ok(Token::Contains) } else {
                    Err(SqlError::Parse(ParseError::new(self.line, self.col, "expected '>' after '@'".into())))
                }
            }
            '?' => Ok(Token::QuestionMark),
            '#' => {
                if self.peek() == Some('>') { self.advance(); Ok(Token::HashArrow) } else {
                    Err(SqlError::Parse(ParseError::new(self.line, self.col, "expected '>' after '#'".into())))
                }
            }
            other => Err(SqlError::Parse(ParseError::new(self.line, self.col, format!("unexpected character '{}'", other)))),
        }
    }
}

fn keyword_or_ident(word: &str) -> Token {
    match word.to_uppercase().as_str() {
        "SELECT" => Token::Select,
        "FROM" => Token::From,
        "WHERE" => Token::Where,
        "JOIN" => Token::Join,
        "ON" => Token::On,
        "AS" => Token::As,
        "GROUP" => Token::Group,
        "ORDER" => Token::Order,
        "BY" => Token::By,
        "HAVING" => Token::Having,
        "INSERT" => Token::Insert,
        "INTO" => Token::Into,
        "VALUES" => Token::Values,
        "UPDATE" => Token::Update,
        "SET" => Token::Set,
        "DELETE" => Token::Delete,
        "CREATE" => Token::Create,
        "TABLE" => Token::Table,
        "DROP" => Token::Drop,
        "ALTER" => Token::Alter,
        "INDEX" => Token::Index,
        "AND" => Token::And,
        "OR" => Token::Or,
        "NOT" => Token::Not,
        "IN" => Token::In,
        "IS" => Token::Is,
        "NULL" => Token::Null,
        "LIKE" => Token::Like,
        "BETWEEN" => Token::Between,
        "EXISTS" => Token::Exists,
        "INNER" => Token::Inner,
        "LEFT" => Token::Left,
        "RIGHT" => Token::Right,
        "FULL" => Token::Full,
        "CROSS" => Token::Cross,
        "OUTER" => Token::Outer,
        "ASC" => Token::Asc,
        "DESC" => Token::Desc,
        "LIMIT" => Token::Limit,
        "OFFSET" => Token::Offset,
        "DISTINCT" => Token::Distinct,
        "ALL" => Token::All,
        "TRUE" => Token::True,
        "FALSE" => Token::False,
        "WITH" => Token::With,
        "RECURSIVE" => Token::Recursive,
        "UNION" => Token::Union,
        "INTERSECT" => Token::Intersect,
        "EXCEPT" => Token::Except,
        "CASE" => Token::Case,
        "WHEN" => Token::When,
        "THEN" => Token::Then,
        "ELSE" => Token::Else,
        "END" => Token::End,
        "CAST" => Token::Cast,
        "PRIMARY" => Token::Primary,
        "KEY" => Token::Key,
        "UNIQUE" => Token::Unique,
        "REFERENCES" => Token::References,
        "DEFAULT" => Token::Default,
        "CHECK" => Token::Check,
        "IF" => Token::If,
        "BEGIN" => Token::Begin,
        "COMMIT" => Token::Commit,
        "ROLLBACK" => Token::Rollback,
        "TRANSACTION" => Token::Transaction,
        "VACUUM" => Token::Vacuum,
        "ANALYZE" => Token::Analyze,
        "EXPLAIN" => Token::Explain,
        "INT" => Token::Int,
        "BIGINT" => Token::Bigint,
        "INTEGER" => Token::Integer,
        "SMALLINT" => Token::Smallint,
        "FLOAT" => Token::Float,
        "DOUBLE" => Token::Double,
        "PRECISION" => Token::Precision,
        "BOOL" => Token::Bool,
        "BOOLEAN" => Token::Boolean,
        "TEXT" => Token::Text,
        "VARCHAR" => Token::Varchar,
        "CHAR" => Token::Char,
        "BYTEA" => Token::Bytea,
        "TIMESTAMP" => Token::Timestamp,
        "DATE" => Token::Date,
        "TIME" => Token::Time,
        "JSON" => Token::Json,
        "JSONB" => Token::Jsonb,
        "VECTOR" => Token::Vector,
        "STORAGE" => Token::Storage,
        "COLUMNAR" => Token::Columnar,
        "ROW" => Token::Row,
        _ => Token::Ident(word.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_select() {
        let tokens = Lexer::tokenize("SELECT id, name FROM users WHERE age > 25").unwrap();
        assert!(tokens.contains(&Token::Select));
        assert!(tokens.contains(&Token::From));
        assert!(tokens.contains(&Token::Where));
        assert!(tokens.contains(&Token::Gt));
        assert!(tokens.contains(&Token::IntLiteral(25)));
    }

    #[test]
    fn test_tokenize_insert() {
        let tokens = Lexer::tokenize("INSERT INTO users (id, name) VALUES (1, 'Alice')").unwrap();
        assert!(tokens.contains(&Token::Insert));
        assert!(tokens.contains(&Token::StringLiteral("Alice".to_string())));
        assert!(tokens.contains(&Token::IntLiteral(1)));
    }

    #[test]
    fn test_tokenize_create_table() {
        let sql = "CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT NOT NULL)";
        let tokens = Lexer::tokenize(sql).unwrap();
        assert!(tokens.contains(&Token::Create));
        assert!(tokens.contains(&Token::Table));
        assert!(tokens.contains(&Token::Primary));
        assert!(tokens.contains(&Token::Key));
    }

    #[test]
    fn test_skip_line_comment() {
        let tokens = Lexer::tokenize("SELECT 1 -- this is a comment\n, 2").unwrap();
        assert_eq!(
            tokens.iter().filter(|t| matches!(t, Token::IntLiteral(_))).count(),
            2
        );
    }

    #[test]
    fn test_string_literal_with_escape() {
        let tokens = Lexer::tokenize("SELECT 'it''s'").unwrap();
        assert!(tokens.contains(&Token::StringLiteral("it's".to_string())));
    }

    #[test]
    fn test_operators() {
        let tokens = Lexer::tokenize("<= >= <> != || ::").unwrap();
        assert!(tokens.contains(&Token::LtEq));
        assert!(tokens.contains(&Token::GtEq));
        assert!(tokens.contains(&Token::NotEq));
        assert!(tokens.contains(&Token::Concat));
        assert!(tokens.contains(&Token::DoubleColon));
    }
}
