/// GIN (Generalized Inverted Index) for JSONB key presence and text tokens.
use crate::heap::TupleId;
use std::collections::{BTreeMap, HashMap, HashSet};

/// GIN index: maps token -> set of TupleIds that contain it.
/// For JSONB: tokens are top-level JSON keys.
/// For text: tokens are words (split on whitespace/punctuation, lowercased).
pub struct GinIndex {
    // posting list: token -> sorted vec of (page_id, slot_id) pairs
    entries: BTreeMap<String, Vec<TupleId>>,
}

impl GinIndex {
    pub fn new() -> Self {
        GinIndex { entries: BTreeMap::new() }
    }

    /// Insert a document: add the TupleId to each token's posting list.
    pub fn insert(&mut self, tid: TupleId, tokens: Vec<String>) {
        // deduplicate tokens for this document
        let unique: HashSet<String> = tokens.into_iter().collect();
        for token in unique {
            let list = self.entries.entry(token).or_default();
            // Insert in sorted order (by page_id then slot_id)
            let pos = list.partition_point(|t| (t.page_id, t.slot_id) < (tid.page_id, tid.slot_id));
            if pos == list.len() || list[pos].page_id != tid.page_id || list[pos].slot_id != tid.slot_id {
                list.insert(pos, tid);
            }
        }
    }

    /// Look up TupleIds for a given token.
    pub fn lookup(&self, token: &str) -> &[TupleId] {
        self.entries.get(token).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Look up TupleIds that contain ALL of the given tokens (intersection).
    pub fn lookup_all(&self, tokens: &[String]) -> Vec<TupleId> {
        if tokens.is_empty() {
            return vec![];
        }
        // Start with the smallest posting list and intersect
        let mut lists: Vec<&[TupleId]> = tokens.iter()
            .map(|t| self.lookup(t.as_str()))
            .collect();
        // Sort by length to start with smallest
        lists.sort_by_key(|l| l.len());

        if lists[0].is_empty() {
            return vec![];
        }

        // Build intersection iteratively
        let mut result: Vec<TupleId> = lists[0].to_vec();
        for list in &lists[1..] {
            result.retain(|tid| {
                list.iter().any(|t| t.page_id == tid.page_id && t.slot_id == tid.slot_id)
            });
            if result.is_empty() {
                return result;
            }
        }
        result
    }

    /// Remove a TupleId from all posting lists (called on DELETE).
    pub fn remove(&mut self, tid: TupleId) {
        for list in self.entries.values_mut() {
            list.retain(|t| !(t.page_id == tid.page_id && t.slot_id == tid.slot_id));
        }
        // Remove empty posting lists
        self.entries.retain(|_, list| !list.is_empty());
    }

    /// Number of unique tokens indexed.
    pub fn token_count(&self) -> usize {
        self.entries.len()
    }

    /// Approximate number of documents indexed (union of all posting lists, deduplicated).
    pub fn document_count(&self) -> usize {
        let mut all: HashSet<(u64, u16)> = HashSet::new();
        for list in self.entries.values() {
            for tid in list {
                all.insert((tid.page_id, tid.slot_id));
            }
        }
        all.len()
    }
}

impl Default for GinIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl GinIndex {
    /// Serialize to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        // magic
        buf.extend_from_slice(b"GIN\0");
        // version
        buf.push(1u8);
        // entry_count
        buf.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        // entries sorted by token (BTreeMap already sorted)
        for (token, posting_list) in &self.entries {
            let token_bytes = token.as_bytes();
            buf.extend_from_slice(&(token_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(token_bytes);
            buf.extend_from_slice(&(posting_list.len() as u32).to_le_bytes());
            for tid in posting_list {
                buf.extend_from_slice(&tid.page_id.to_le_bytes());
                buf.extend_from_slice(&tid.slot_id.to_le_bytes());
            }
        }
        buf
    }

    /// Deserialize from bytes. Returns Err if format is wrong.
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let mut pos = 0usize;

        macro_rules! need {
            ($n:expr) => {
                if pos + $n > data.len() {
                    return Err(format!("truncated at offset {}", pos));
                }
            };
        }
        macro_rules! read_u8 {
            () => {{
                need!(1);
                let v = data[pos];
                pos += 1;
                v
            }};
        }
        macro_rules! read_u16 {
            () => {{
                need!(2);
                let v = u16::from_le_bytes(data[pos..pos + 2].try_into().unwrap());
                pos += 2;
                v
            }};
        }
        macro_rules! read_u32 {
            () => {{
                need!(4);
                let v = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
                pos += 4;
                v
            }};
        }
        macro_rules! read_u64 {
            () => {{
                need!(8);
                let v = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
                pos += 8;
                v
            }};
        }

        // magic
        need!(4);
        if &data[pos..pos + 4] != b"GIN\0" {
            return Err("bad magic".to_string());
        }
        pos += 4;

        let version = read_u8!();
        if version != 1 {
            return Err(format!("unsupported version {}", version));
        }

        let entry_count = read_u32!() as usize;
        let mut entries = BTreeMap::new();

        for _ in 0..entry_count {
            let token_len = read_u32!() as usize;
            need!(token_len);
            let token = std::str::from_utf8(&data[pos..pos + token_len])
                .map_err(|e| format!("invalid token UTF-8: {}", e))?
                .to_string();
            pos += token_len;

            let posting_count = read_u32!() as usize;
            let mut posting_list = Vec::with_capacity(posting_count);
            for _ in 0..posting_count {
                let page_id = read_u64!();
                let slot_id = read_u16!();
                posting_list.push(TupleId { page_id, slot_id });
            }
            entries.insert(token, posting_list);
        }

        Ok(GinIndex { entries })
    }

    /// Save to a file path.
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let bytes = self.to_bytes();
        std::fs::write(path, &bytes)
    }

    /// Load from a file path.
    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::from_bytes(&bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// Extract top-level keys from a JSONB binary value as tokens.
pub fn jsonb_tokens(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return vec![];
    }
    // First byte should be TAG_OBJECT (0x08)
    if bytes[0] != 0x08 {
        return vec![];
    }
    if bytes.len() < 5 {
        return vec![];
    }
    let count = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) as usize;
    let mut pos = 5;
    let mut keys = Vec::with_capacity(count);

    // Build a reverse mapping from the jsonb module tags
    for _ in 0..count {
        // key is a TAG_STRING (0x06)
        if pos >= bytes.len() || bytes[pos] != 0x06 {
            break;
        }
        pos += 1;
        if pos + 4 > bytes.len() {
            break;
        }
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        if pos + len > bytes.len() {
            break;
        }
        if let Ok(s) = std::str::from_utf8(&bytes[pos..pos + len]) {
            keys.push(s.to_string());
        }
        pos += len;
        // Skip the value - we need to decode it to know its size
        pos = skip_jsonb_value(bytes, pos);
    }
    keys
}

/// Skip over one JSONB value at `pos`, returning the new position.
fn skip_jsonb_value(bytes: &[u8], pos: usize) -> usize {
    if pos >= bytes.len() {
        return pos;
    }
    let tag = bytes[pos];
    let mut p = pos + 1;
    match tag {
        0x01..=0x03 => p, // null, false, true
        0x04 | 0x05 => p + 8, // int64 or float64
        0x06 => {
            // string: 4 bytes len + data
            if p + 4 > bytes.len() { return p; }
            let len = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
            p + 4 + len
        }
        0x07 => {
            // array: 4 bytes count + items
            if p + 4 > bytes.len() { return p; }
            let count = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
            p += 4;
            for _ in 0..count {
                p = skip_jsonb_value(bytes, p);
            }
            p
        }
        0x08 => {
            // object: 4 bytes count + key-value pairs
            if p + 4 > bytes.len() { return p; }
            let count = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
            p += 4;
            for _ in 0..count {
                p = skip_jsonb_value(bytes, p); // key
                p = skip_jsonb_value(bytes, p); // value
            }
            p
        }
        _ => p,
    }
}

/// Extract words from a text value as tokens (split on non-alphanumeric chars, lowercase).
pub fn text_tokens(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            current.push(c.to_lowercase().next().unwrap());
        } else if !current.is_empty() {
            tokens.push(current.clone());
            current.clear();
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Internal helper: track token-to-doc mapping for document_count
#[allow(dead_code)]
fn unique_tids(entries: &BTreeMap<String, Vec<TupleId>>) -> HashMap<(u64, u16), ()> {
    let mut all = HashMap::new();
    for list in entries.values() {
        for tid in list {
            all.insert((tid.page_id, tid.slot_id), ());
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::TupleId;
    use crate::jsonb::encode_jsonb;

    fn make_tid(page: u64, slot: u16) -> TupleId {
        TupleId { page_id: page, slot_id: slot }
    }

    #[test]
    fn test_gin_insert_lookup() {
        let mut gin = GinIndex::new();
        let t1 = make_tid(1, 0);
        let t2 = make_tid(2, 0);
        let t3 = make_tid(3, 0);

        gin.insert(t1, vec!["foo".into(), "bar".into()]);
        gin.insert(t2, vec!["bar".into(), "baz".into()]);
        gin.insert(t3, vec!["qux".into()]);

        let foo_results = gin.lookup("foo");
        assert_eq!(foo_results.len(), 1);
        assert_eq!(foo_results[0].page_id, 1);

        let bar_results = gin.lookup("bar");
        assert_eq!(bar_results.len(), 2);

        let baz_results = gin.lookup("baz");
        assert_eq!(baz_results.len(), 1);
        assert_eq!(baz_results[0].page_id, 2);

        assert!(gin.lookup("missing").is_empty());
    }

    #[test]
    fn test_gin_lookup_all() {
        let mut gin = GinIndex::new();
        let t1 = make_tid(1, 0);
        let t2 = make_tid(2, 0);
        let t3 = make_tid(3, 0);

        gin.insert(t1, vec!["a".into(), "b".into(), "c".into()]);
        gin.insert(t2, vec!["a".into(), "b".into()]);
        gin.insert(t3, vec!["a".into()]);

        // Only t1 has all three
        let results = gin.lookup_all(&["a".into(), "b".into(), "c".into()]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].page_id, 1);

        // t1 and t2 both have a and b
        let results2 = gin.lookup_all(&["a".into(), "b".into()]);
        assert_eq!(results2.len(), 2);

        // no one has "z"
        let results3 = gin.lookup_all(&["a".into(), "z".into()]);
        assert!(results3.is_empty());
    }

    #[test]
    fn test_gin_remove() {
        let mut gin = GinIndex::new();
        let t1 = make_tid(1, 0);
        let t2 = make_tid(2, 0);

        gin.insert(t1, vec!["foo".into()]);
        gin.insert(t2, vec!["foo".into()]);
        assert_eq!(gin.lookup("foo").len(), 2);

        gin.remove(t1);
        let results = gin.lookup("foo");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].page_id, 2);
    }

    #[test]
    fn test_jsonb_tokens() {
        let json = r#"{"a":1,"b":2,"c":{"d":3}}"#;
        let encoded = encode_jsonb(json).unwrap();
        let tokens = jsonb_tokens(&encoded);
        assert_eq!(tokens.len(), 3);
        assert!(tokens.contains(&"a".to_string()));
        assert!(tokens.contains(&"b".to_string()));
        assert!(tokens.contains(&"c".to_string()));
        // nested "d" should NOT be in top-level tokens
        assert!(!tokens.contains(&"d".to_string()));
    }

    #[test]
    fn test_text_tokens() {
        let tokens = text_tokens("Hello World, foo-bar");
        assert_eq!(tokens, vec!["hello", "world", "foo", "bar"]);
    }

    #[test]
    fn test_gin_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gin.bin");

        let mut gin = GinIndex::new();
        let t1 = make_tid(1, 0);
        let t2 = make_tid(2, 1);
        let t3 = make_tid(3, 2);

        gin.insert(t1, vec!["alpha".into(), "beta".into()]);
        gin.insert(t2, vec!["beta".into(), "gamma".into()]);
        gin.insert(t3, vec!["delta".into()]);

        gin.save(&path).unwrap();
        let loaded = GinIndex::load(&path).unwrap();

        // Verify token count
        assert_eq!(loaded.token_count(), gin.token_count());

        // Verify lookups return same TupleIds
        let alpha = loaded.lookup("alpha");
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].page_id, 1);

        let beta = loaded.lookup("beta");
        assert_eq!(beta.len(), 2);

        let gamma = loaded.lookup("gamma");
        assert_eq!(gamma.len(), 1);
        assert_eq!(gamma[0].page_id, 2);

        let delta = loaded.lookup("delta");
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].page_id, 3);

        assert!(loaded.lookup("missing").is_empty());
    }

    #[test]
    fn test_gin_empty_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gin_empty.bin");

        let gin = GinIndex::new();
        gin.save(&path).unwrap();

        let loaded = GinIndex::load(&path).unwrap();
        assert_eq!(loaded.token_count(), 0);
        assert!(loaded.lookup("anything").is_empty());
    }
}
