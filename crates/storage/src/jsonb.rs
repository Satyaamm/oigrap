/// JSONB binary format: a self-describing byte sequence.
/// Layout:
///   1 byte: type tag
///   followed by encoded value
///
/// Tags:
///   0x01 = Null
///   0x02 = Bool(false)
///   0x03 = Bool(true)
///   0x04 = Int64:  8 bytes LE
///   0x05 = Float64: 8 bytes LE
///   0x06 = String: 4 bytes len LE + bytes (UTF-8)
///   0x07 = Array: 4 bytes count LE + [encoded values...]
///   0x08 = Object: 4 bytes count LE + [encoded key-value pairs...]
///            where each pair is: encoded String key + encoded value
const TAG_NULL: u8 = 0x01;
const TAG_BOOL_FALSE: u8 = 0x02;
const TAG_BOOL_TRUE: u8 = 0x03;
const TAG_INT64: u8 = 0x04;
const TAG_FLOAT64: u8 = 0x05;
const TAG_STRING: u8 = 0x06;
const TAG_ARRAY: u8 = 0x07;
const TAG_OBJECT: u8 = 0x08;

/// A minimal JSON value representation for internal use.
#[derive(Debug, PartialEq)]
enum JsonVal {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<JsonVal>),
    Object(Vec<(String, JsonVal)>),
}

// ---- encoding ----

fn encode_val(val: &JsonVal, out: &mut Vec<u8>) {
    match val {
        JsonVal::Null => out.push(TAG_NULL),
        JsonVal::Bool(false) => out.push(TAG_BOOL_FALSE),
        JsonVal::Bool(true) => out.push(TAG_BOOL_TRUE),
        JsonVal::Int(n) => {
            out.push(TAG_INT64);
            out.extend_from_slice(&n.to_le_bytes());
        }
        JsonVal::Float(f) => {
            out.push(TAG_FLOAT64);
            out.extend_from_slice(&f.to_le_bytes());
        }
        JsonVal::Str(s) => {
            out.push(TAG_STRING);
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        JsonVal::Array(items) => {
            out.push(TAG_ARRAY);
            out.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for item in items {
                encode_val(item, out);
            }
        }
        JsonVal::Object(pairs) => {
            out.push(TAG_OBJECT);
            out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
            for (k, v) in pairs {
                // key is always a string
                let key_val = JsonVal::Str(k.clone());
                encode_val(&key_val, out);
                encode_val(v, out);
            }
        }
    }
}

/// Encode JSON text to JSONB binary format.
pub fn encode_jsonb(json_str: &str) -> Result<Vec<u8>, String> {
    let val = parse_json(json_str.trim()).map_err(|e| format!("JSON parse error: {}", e))?;
    let mut out = Vec::new();
    encode_val(&val, &mut out);
    Ok(out)
}

// ---- decoding ----

fn decode_val(bytes: &[u8], pos: &mut usize) -> Result<JsonVal, String> {
    if *pos >= bytes.len() {
        return Err("unexpected end of JSONB data".into());
    }
    let tag = bytes[*pos];
    *pos += 1;
    match tag {
        TAG_NULL => Ok(JsonVal::Null),
        TAG_BOOL_FALSE => Ok(JsonVal::Bool(false)),
        TAG_BOOL_TRUE => Ok(JsonVal::Bool(true)),
        TAG_INT64 => {
            if *pos + 8 > bytes.len() {
                return Err("truncated int64".into());
            }
            let n = i64::from_le_bytes(bytes[*pos..*pos + 8].try_into().unwrap());
            *pos += 8;
            Ok(JsonVal::Int(n))
        }
        TAG_FLOAT64 => {
            if *pos + 8 > bytes.len() {
                return Err("truncated float64".into());
            }
            let f = f64::from_le_bytes(bytes[*pos..*pos + 8].try_into().unwrap());
            *pos += 8;
            Ok(JsonVal::Float(f))
        }
        TAG_STRING => {
            if *pos + 4 > bytes.len() {
                return Err("truncated string length".into());
            }
            let len = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().unwrap()) as usize;
            *pos += 4;
            if *pos + len > bytes.len() {
                return Err("truncated string data".into());
            }
            let s = std::str::from_utf8(&bytes[*pos..*pos + len])
                .map_err(|e| format!("invalid UTF-8: {}", e))?
                .to_string();
            *pos += len;
            Ok(JsonVal::Str(s))
        }
        TAG_ARRAY => {
            if *pos + 4 > bytes.len() {
                return Err("truncated array count".into());
            }
            let count = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().unwrap()) as usize;
            *pos += 4;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(decode_val(bytes, pos)?);
            }
            Ok(JsonVal::Array(items))
        }
        TAG_OBJECT => {
            if *pos + 4 > bytes.len() {
                return Err("truncated object count".into());
            }
            let count = u32::from_le_bytes(bytes[*pos..*pos + 4].try_into().unwrap()) as usize;
            *pos += 4;
            let mut pairs = Vec::with_capacity(count);
            for _ in 0..count {
                let key = decode_val(bytes, pos)?;
                let key_str = if let JsonVal::Str(s) = key {
                    s
                } else {
                    return Err("object key must be a string".into());
                };
                let val = decode_val(bytes, pos)?;
                pairs.push((key_str, val));
            }
            Ok(JsonVal::Object(pairs))
        }
        other => Err(format!("unknown JSONB tag: {}", other)),
    }
}

/// Decode JSONB binary to JSON text.
pub fn decode_jsonb(bytes: &[u8]) -> Result<String, String> {
    let mut pos = 0;
    let val = decode_val(bytes, &mut pos)?;
    Ok(format_json(&val))
}

fn format_json(val: &JsonVal) -> String {
    match val {
        JsonVal::Null => "null".into(),
        JsonVal::Bool(b) => if *b { "true".into() } else { "false".into() },
        JsonVal::Int(n) => n.to_string(),
        JsonVal::Float(f) => {
            // Use a compact representation
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{:.1}", f)
            } else {
                f.to_string()
            }
        }
        JsonVal::Str(s) => format!("\"{}\"", escape_json_str(s)),
        JsonVal::Array(items) => {
            let inner: Vec<String> = items.iter().map(format_json).collect();
            format!("[{}]", inner.join(","))
        }
        JsonVal::Object(pairs) => {
            let inner: Vec<String> = pairs.iter()
                .map(|(k, v)| format!("\"{}\":{}", escape_json_str(k), format_json(v)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
    }
}

fn escape_json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

// ---- key/index access ----

/// Get an object value by key (returns the JSONB-encoded value).
pub fn jsonb_get_key(bytes: &[u8], key: &str) -> Option<Vec<u8>> {
    let mut pos = 0;
    let val = decode_val(bytes, &mut pos).ok()?;
    if let JsonVal::Object(pairs) = val {
        for (k, v) in pairs {
            if k == key {
                let mut out = Vec::new();
                encode_val(&v, &mut out);
                return Some(out);
            }
        }
    }
    None
}

/// Get an array element by index (returns the JSONB-encoded value).
pub fn jsonb_get_idx(bytes: &[u8], idx: usize) -> Option<Vec<u8>> {
    let mut pos = 0;
    let val = decode_val(bytes, &mut pos).ok()?;
    if let JsonVal::Array(items) = val {
        if let Some(item) = items.into_iter().nth(idx) {
            let mut out = Vec::new();
            encode_val(&item, &mut out);
            return Some(out);
        }
    }
    None
}

/// Containment check: does `outer` @> `inner`?
/// For objects: every key-value pair in inner must exist in outer (recursively).
/// For arrays: every element in inner must appear in outer.
/// For scalars: must be equal.
pub fn jsonb_contains(outer: &[u8], inner: &[u8]) -> bool {
    let mut p1 = 0;
    let mut p2 = 0;
    let outer_val = match decode_val(outer, &mut p1) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let inner_val = match decode_val(inner, &mut p2) {
        Ok(v) => v,
        Err(_) => return false,
    };
    json_contains_val(&outer_val, &inner_val)
}

fn json_contains_val(outer: &JsonVal, inner: &JsonVal) -> bool {
    match (outer, inner) {
        (JsonVal::Object(o_pairs), JsonVal::Object(i_pairs)) => {
            for (ik, iv) in i_pairs {
                let found = o_pairs.iter().any(|(ok, ov)| ok == ik && json_contains_val(ov, iv));
                if !found {
                    return false;
                }
            }
            true
        }
        (JsonVal::Array(o_items), JsonVal::Array(i_items)) => {
            for iv in i_items {
                let found = o_items.iter().any(|ov| json_contains_val(ov, iv));
                if !found {
                    return false;
                }
            }
            true
        }
        // An array can contain a scalar
        (JsonVal::Array(o_items), iv) => {
            o_items.iter().any(|ov| json_contains_val(ov, iv))
        }
        (a, b) => a == b,
    }
}

/// Extract value as plain text (no JSON quotes around strings).
pub fn jsonb_to_text(bytes: &[u8]) -> String {
    let mut pos = 0;
    match decode_val(bytes, &mut pos) {
        Ok(JsonVal::Str(s)) => s,
        Ok(val) => format_json(&val),
        Err(_) => String::new(),
    }
}

// ---- minimal JSON parser ----

struct JsonParser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a str) -> Self {
        JsonParser { input: input.as_bytes(), pos: 0 }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.input.len() && (self.input[self.pos] as char).is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.input.get(self.pos).map(|&b| b as char)
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.input.get(self.pos).map(|&b| b as char);
        self.pos += 1;
        c
    }

    fn parse_value(&mut self) -> Result<JsonVal, String> {
        self.skip_ws();
        match self.peek() {
            Some('"') => self.parse_string().map(JsonVal::Str),
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('t') => {
                self.expect_literal(b"true")?;
                Ok(JsonVal::Bool(true))
            }
            Some('f') => {
                self.expect_literal(b"false")?;
                Ok(JsonVal::Bool(false))
            }
            Some('n') => {
                self.expect_literal(b"null")?;
                Ok(JsonVal::Null)
            }
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number(),
            other => Err(format!("unexpected character: {:?}", other)),
        }
    }

    fn expect_literal(&mut self, lit: &[u8]) -> Result<(), String> {
        for &b in lit {
            match self.advance() {
                Some(c) if c as u8 == b => {}
                other => return Err(format!("expected '{}', got {:?}", b as char, other)),
            }
        }
        Ok(())
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.advance(); // consume opening "
        let mut s = String::new();
        loop {
            match self.advance() {
                None => return Err("unterminated string".into()),
                Some('"') => break,
                Some('\\') => {
                    match self.advance() {
                        Some('"') => s.push('"'),
                        Some('\\') => s.push('\\'),
                        Some('/') => s.push('/'),
                        Some('n') => s.push('\n'),
                        Some('r') => s.push('\r'),
                        Some('t') => s.push('\t'),
                        Some('b') => s.push('\x08'),
                        Some('f') => s.push('\x0C'),
                        Some('u') => {
                            // Parse 4 hex digits
                            let mut hex = String::new();
                            for _ in 0..4 {
                                match self.advance() {
                                    Some(c) => hex.push(c),
                                    None => return Err("truncated unicode escape".into()),
                                }
                            }
                            let code = u32::from_str_radix(&hex, 16)
                                .map_err(|_| format!("invalid unicode escape: \\u{}", hex))?;
                            let ch = char::from_u32(code)
                                .ok_or_else(|| format!("invalid unicode codepoint: {}", code))?;
                            s.push(ch);
                        }
                        other => return Err(format!("invalid escape: {:?}", other)),
                    }
                }
                Some(c) => s.push(c),
            }
        }
        Ok(s)
    }

    fn parse_number(&mut self) -> Result<JsonVal, String> {
        let start = self.pos;
        let mut is_float = false;

        if self.peek() == Some('-') {
            self.pos += 1;
        }
        // integer part
        while self.pos < self.input.len() && (self.input[self.pos] as char).is_ascii_digit() {
            self.pos += 1;
        }
        // fractional
        if self.pos < self.input.len() && self.input[self.pos] == b'.' {
            is_float = true;
            self.pos += 1;
            while self.pos < self.input.len() && (self.input[self.pos] as char).is_ascii_digit() {
                self.pos += 1;
            }
        }
        // exponent
        if self.pos < self.input.len() && (self.input[self.pos] == b'e' || self.input[self.pos] == b'E') {
            is_float = true;
            self.pos += 1;
            if self.pos < self.input.len() && (self.input[self.pos] == b'+' || self.input[self.pos] == b'-') {
                self.pos += 1;
            }
            while self.pos < self.input.len() && (self.input[self.pos] as char).is_ascii_digit() {
                self.pos += 1;
            }
        }

        let s = std::str::from_utf8(&self.input[start..self.pos]).unwrap();
        if is_float {
            s.parse::<f64>().map(JsonVal::Float).map_err(|e| e.to_string())
        } else {
            s.parse::<i64>().map(JsonVal::Int).map_err(|e| e.to_string())
        }
    }

    fn parse_object(&mut self) -> Result<JsonVal, String> {
        self.advance(); // consume '{'
        self.skip_ws();
        let mut pairs = Vec::new();
        if self.peek() == Some('}') {
            self.advance();
            return Ok(JsonVal::Object(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            match self.advance() {
                Some(':') => {}
                other => return Err(format!("expected ':', got {:?}", other)),
            }
            self.skip_ws();
            let val = self.parse_value()?;
            pairs.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(',') => { self.advance(); }
                Some('}') => { self.advance(); break; }
                other => return Err(format!("expected ',' or '}}', got {:?}", other)),
            }
        }
        Ok(JsonVal::Object(pairs))
    }

    fn parse_array(&mut self) -> Result<JsonVal, String> {
        self.advance(); // consume '['
        self.skip_ws();
        let mut items = Vec::new();
        if self.peek() == Some(']') {
            self.advance();
            return Ok(JsonVal::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(',') => { self.advance(); }
                Some(']') => { self.advance(); break; }
                other => return Err(format!("expected ',' or ']', got {:?}", other)),
            }
        }
        Ok(JsonVal::Array(items))
    }
}

fn parse_json(s: &str) -> Result<JsonVal, String> {
    let mut p = JsonParser::new(s);
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.input.len() {
        return Err(format!("trailing garbage at position {}", p.pos));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_object() {
        let json = r#"{"name":"Alice","age":30}"#;
        let encoded = encode_jsonb(json).unwrap();
        let decoded = decode_jsonb(&encoded).unwrap();
        // Re-encode the decoded result to normalize whitespace, then compare
        let re_encoded = encode_jsonb(&decoded).unwrap();
        assert_eq!(encoded, re_encoded);
        // Also verify the decoded contains expected content
        assert!(decoded.contains("Alice"));
        assert!(decoded.contains("name"));
        assert!(decoded.contains("30"));
    }

    #[test]
    fn test_encode_decode_array() {
        let json = "[1,2,3]";
        let encoded = encode_jsonb(json).unwrap();
        let decoded = decode_jsonb(&encoded).unwrap();
        assert!(decoded.contains('1'));
        assert!(decoded.contains('2'));
        assert!(decoded.contains('3'));
        let re_encoded = encode_jsonb(&decoded).unwrap();
        assert_eq!(encoded, re_encoded);
    }

    #[test]
    fn test_get_key() {
        let json = r#"{"name":"Alice","age":30}"#;
        let encoded = encode_jsonb(json).unwrap();
        let val = jsonb_get_key(&encoded, "name").unwrap();
        let text = jsonb_to_text(&val);
        assert_eq!(text, "Alice");

        let age_val = jsonb_get_key(&encoded, "age").unwrap();
        let age_text = jsonb_to_text(&age_val);
        assert_eq!(age_text, "30");

        assert!(jsonb_get_key(&encoded, "missing").is_none());
    }

    #[test]
    fn test_get_idx() {
        let json = "[10,20,30]";
        let encoded = encode_jsonb(json).unwrap();
        let v0 = jsonb_get_idx(&encoded, 0).unwrap();
        assert_eq!(jsonb_to_text(&v0), "10");
        let v2 = jsonb_get_idx(&encoded, 2).unwrap();
        assert_eq!(jsonb_to_text(&v2), "30");
        assert!(jsonb_get_idx(&encoded, 10).is_none());
    }

    #[test]
    fn test_contains_object() {
        let outer = encode_jsonb(r#"{"a":1,"b":2}"#).unwrap();
        let inner1 = encode_jsonb(r#"{"a":1}"#).unwrap();
        let inner2 = encode_jsonb(r#"{"a":2}"#).unwrap();
        assert!(jsonb_contains(&outer, &inner1));
        assert!(!jsonb_contains(&outer, &inner2));
    }

    #[test]
    fn test_nested() {
        let json = r#"{"user":{"id":1,"tags":["a","b"]}}"#;
        let encoded = encode_jsonb(json).unwrap();
        let user_val = jsonb_get_key(&encoded, "user").unwrap();
        let id_val = jsonb_get_key(&user_val, "id").unwrap();
        assert_eq!(jsonb_to_text(&id_val), "1");
        let tags_val = jsonb_get_key(&user_val, "tags").unwrap();
        let tag0 = jsonb_get_idx(&tags_val, 0).unwrap();
        assert_eq!(jsonb_to_text(&tag0), "a");
    }
}
