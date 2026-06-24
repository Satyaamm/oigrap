use crate::ast::DataType;
use crate::error::{Result, SqlError};
use std::fmt;

/// A runtime SQL value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(f64),
    Text(String),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Int64(_) => "int64",
            Value::Float64(_) => "float64",
            Value::Text(_) => "text",
        }
    }

    pub fn coerce_to(&self, target: &DataType) -> Result<Value> {
        match (self, target) {
            (Value::Null, _) => Ok(Value::Null),
            (Value::Int64(n), DataType::Int64 | DataType::Int32 | DataType::Int16) => Ok(Value::Int64(*n)),
            (Value::Int64(n), DataType::Float64 | DataType::Float32) => Ok(Value::Float64(*n as f64)),
            (Value::Float64(f), DataType::Float64 | DataType::Float32) => Ok(Value::Float64(*f)),
            (Value::Bool(b), DataType::Boolean) => Ok(Value::Bool(*b)),
            (Value::Text(s), DataType::Text | DataType::Varchar(_) | DataType::Char(_)) => Ok(Value::Text(s.clone())),
            _ => Err(SqlError::Semantic(format!(
                "cannot coerce {} to {:?}", self.type_name(), target
            ))),
        }
    }

    /// Encode this value as bytes for B+ tree key lookup.
    /// Int64 → 8 bytes big-endian (preserves ordering).
    /// Text → UTF-8 bytes.
    pub fn as_key_bytes(&self) -> Option<Vec<u8>> {
        match self {
            Value::Int64(n) => {
                // Flip sign bit so negative numbers sort below positive
                let bits = (*n as u64) ^ (1u64 << 63);
                Some(bits.to_be_bytes().to_vec())
            }
            Value::Text(s) => Some(s.as_bytes().to_vec()),
            Value::Bool(b) => Some(vec![if *b { 1 } else { 0 }]),
            _ => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Bool(b) => write!(f, "{}", if *b { "t" } else { "f" }),
            Value::Int64(n) => write!(f, "{}", n),
            Value::Float64(v) => write!(f, "{}", v),
            Value::Text(s) => write!(f, "{}", s),
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Value::Null, Value::Null) => Some(std::cmp::Ordering::Equal),
            (Value::Null, _) | (_, Value::Null) => None,
            (Value::Int64(a), Value::Int64(b)) => a.partial_cmp(b),
            (Value::Float64(a), Value::Float64(b)) => a.partial_cmp(b),
            (Value::Int64(a), Value::Float64(b)) => (*a as f64).partial_cmp(b),
            (Value::Float64(a), Value::Int64(b)) => a.partial_cmp(&(*b as f64)),
            (Value::Text(a), Value::Text(b)) => a.partial_cmp(b),
            (Value::Bool(a), Value::Bool(b)) => a.partial_cmp(b),
            _ => None,
        }
    }
}
