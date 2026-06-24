/// Row encoding: null_bitmap (ceil(ncols/8) bytes) + column data.
/// This is the payload written after the 24-byte MVCC header in the heap.
use crate::catalog::ColumnSchema;
use crate::error::{Result, SqlError};
use crate::value::Value;

/// Encode a row's column values into bytes (no MVCC header).
pub fn encode_row(schema: &[ColumnSchema], values: &[Value]) -> Result<Vec<u8>> {
    assert_eq!(schema.len(), values.len());
    let ncols = values.len();

    let bitmap_bytes = ncols.div_ceil(8);
    let mut bitmap = vec![0u8; bitmap_bytes];
    let mut data: Vec<u8> = Vec::new();

    for (i, val) in values.iter().enumerate() {
        if !val.is_null() {
            bitmap[i / 8] |= 1 << (i % 8);
            encode_value(val, &mut data)?;
        }
    }

    let mut out = bitmap;
    out.extend_from_slice(&data);
    Ok(out)
}

/// Decode a row from raw bytes (no MVCC header).
pub fn decode_row(schema: &[ColumnSchema], bytes: &[u8]) -> Result<Vec<Value>> {
    let ncols = schema.len();
    let bitmap_bytes = ncols.div_ceil(8);

    if bytes.len() < bitmap_bytes {
        return Err(SqlError::Execution("row bytes too short for null bitmap".into()));
    }

    let bitmap = &bytes[..bitmap_bytes];
    let mut pos = bitmap_bytes;
    let mut values = Vec::with_capacity(ncols);

    for (i, col) in schema.iter().enumerate() {
        let is_non_null = (bitmap[i / 8] >> (i % 8)) & 1 == 1;
        if is_non_null {
            let (val, consumed) = decode_value(&col.sql_type, &bytes[pos..])?;
            pos += consumed;
            values.push(val);
        } else {
            values.push(Value::Null);
        }
    }

    Ok(values)
}

fn encode_value(val: &Value, buf: &mut Vec<u8>) -> Result<()> {
    match val {
        Value::Bool(b) => buf.push(if *b { 1 } else { 0 }),
        Value::Int64(n) => buf.extend_from_slice(&n.to_le_bytes()),
        Value::Float64(f) => buf.extend_from_slice(&f.to_bits().to_le_bytes()),
        Value::Text(s) => {
            let bytes = s.as_bytes();
            buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(bytes);
        }
        Value::Null => {}
    }
    Ok(())
}

use crate::catalog::SqlType;

fn decode_value(t: &SqlType, bytes: &[u8]) -> Result<(Value, usize)> {
    match t {
        SqlType::Boolean => {
            if bytes.is_empty() {
                return Err(SqlError::Execution("truncated boolean".into()));
            }
            Ok((Value::Bool(bytes[0] != 0), 1))
        }
        SqlType::Int64 => {
            if bytes.len() < 8 {
                return Err(SqlError::Execution("truncated int64".into()));
            }
            let n = i64::from_le_bytes(bytes[..8].try_into().unwrap());
            Ok((Value::Int64(n), 8))
        }
        SqlType::Float64 => {
            if bytes.len() < 8 {
                return Err(SqlError::Execution("truncated float64".into()));
            }
            let bits = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            Ok((Value::Float64(f64::from_bits(bits)), 8))
        }
        SqlType::Text => {
            if bytes.len() < 4 {
                return Err(SqlError::Execution("truncated text length".into()));
            }
            let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
            if bytes.len() < 4 + len {
                return Err(SqlError::Execution("truncated text data".into()));
            }
            let s = std::str::from_utf8(&bytes[4..4 + len])
                .map_err(|_| SqlError::Execution("invalid UTF-8 in text column".into()))?
                .to_string();
            Ok((Value::Text(s), 4 + len))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnSchema, SqlType};

    fn schema(types: &[SqlType]) -> Vec<ColumnSchema> {
        types.iter().enumerate().map(|(i, t)| ColumnSchema {
            name: format!("c{}", i),
            sql_type: t.clone(),
            nullable: true,
            primary_key: false,
        }).collect()
    }

    #[test]
    fn test_round_trip_basic_types() {
        let s = schema(&[SqlType::Int64, SqlType::Text, SqlType::Boolean, SqlType::Float64]);
        let values = vec![
            Value::Int64(42),
            Value::Text("hello".into()),
            Value::Bool(true),
            Value::Float64(3.14),
        ];
        let bytes = encode_row(&s, &values).unwrap();
        let decoded = decode_row(&s, &bytes).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_round_trip_with_nulls() {
        let s = schema(&[SqlType::Int64, SqlType::Text, SqlType::Int64]);
        let values = vec![Value::Int64(1), Value::Null, Value::Int64(3)];
        let bytes = encode_row(&s, &values).unwrap();
        let decoded = decode_row(&s, &bytes).unwrap();
        assert_eq!(decoded, values);
    }
}
