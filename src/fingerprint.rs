//! Stable fingerprinting of model-query input messages for replay drift detection.
//!
//! Each model call's input message vector is canonically serialized (compact JSON,
//! alphabetically-sorted keys, only `role` + `content` — metadata-free) and hashed
//! with SHA-256. The first 8 bytes are returned as a 16-hex-char string together
//! with the byte length of the canonical form.
//!
//! Only `role` and `content` are included; `cache_hint` and `extra` are intentionally
//! excluded because they carry per-run metadata (cost, timestamps, raw API response)
//! that legitimately differs between the recording run and a replay.

use sha2::{Digest, Sha256};

use crate::model::Message;

/// Fingerprint of a model-query input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputFingerprint {
    /// SHA-256 of the canonical JSON, first 8 bytes as lowercase hex (16 chars).
    pub hex: String,
    /// Byte length of the canonical JSON string.
    pub canonical_size: usize,
}

/// Compute a stable fingerprint for a slice of messages.
///
/// The canonical form is compact JSON (no extra whitespace) where each message
/// is represented as `{"content": "…", "role": "…"}` (keys alphabetically sorted,
/// no other fields). The fingerprint is SHA-256 over that UTF-8 string, truncated
/// to the first 8 bytes and hex-encoded.
pub fn compute_input_fingerprint(messages: &[Message]) -> InputFingerprint {
    let canonical = canonical_json(messages);
    let canonical_size = canonical.len();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let hash = hasher.finalize();
    let mut hex = String::with_capacity(16);
    for b in &hash[..8] {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    InputFingerprint { hex, canonical_size }
}

/// Produce the canonical JSON string for a slice of messages.
///
/// Each message becomes `{"content": <string>, "role": <string>}`. Keys are in
/// ASCII-alphabetical order ("content" < "role"). The array is compact (no
/// indentation or trailing whitespace). `extra` and `cache_hint` are excluded.
///
/// This function is `pub` so tests can inspect and assert on the canonical form.
pub fn canonical_json(messages: &[Message]) -> String {
    use serde_json::{Map, Value};
    let arr: Vec<Value> = messages
        .iter()
        .map(|m| {
            let mut obj = Map::new();
            // Alphabetical: "content" < "role" — insert in that order so IndexMap
            // preserves insertion order in the serialized output.
            obj.insert("content".to_owned(), Value::String(m.content.clone()));
            obj.insert("role".to_owned(), role_to_json_str(m.role));
            Value::Object(obj)
        })
        .collect();
    // serde_json::to_string on a Vec<Value> produces compact JSON.
    serde_json::to_string(&Value::Array(arr))
        .unwrap_or_else(|_| "[]".to_owned())
}

/// Cap a canonical JSON string to `cap` bytes, appending `[truncated]` if cut.
///
/// Returns the (possibly truncated) string and a bool indicating truncation.
pub fn cap_canonical(s: &str, cap: usize) -> (String, bool) {
    if s.len() <= cap {
        return (s.to_owned(), false);
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}[truncated]", &s[..end]), true)
}

fn role_to_json_str(role: crate::model::Role) -> serde_json::Value {
    use crate::model::Role;
    serde_json::Value::String(
        match role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
        .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::model::{Message, Role};

    #[test]
    fn fingerprint_is_16_hex_chars() {
        let fp = compute_input_fingerprint(&[Message::user("hello")]);
        assert_eq!(fp.hex.len(), 16);
        assert!(fp.hex.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn same_input_same_fingerprint() {
        let msgs = vec![Message::user("a"), Message::assistant("b")];
        assert_eq!(
            compute_input_fingerprint(&msgs).hex,
            compute_input_fingerprint(&msgs).hex
        );
    }

    #[test]
    fn different_content_different_fingerprint() {
        let fp1 = compute_input_fingerprint(&[Message::user("hello")]);
        let fp2 = compute_input_fingerprint(&[Message::user("world")]);
        assert_ne!(fp1.hex, fp2.hex);
    }

    #[test]
    fn canonical_json_sorted_keys() {
        let json = canonical_json(&[Message::user("x")]);
        let cp = json.find("\"content\"").unwrap();
        let rp = json.find("\"role\"").unwrap();
        assert!(cp < rp);
    }

    #[test]
    fn canonical_json_excludes_extra_and_cache_hint() {
        let mut m = Message::user("hi");
        m.extra.cost = Some(9.99);
        m.extra.timestamp = Some("ts".into());
        m.cache_hint = crate::model::CacheHint::Breakpoint;
        let json = canonical_json(&[m]);
        assert!(!json.contains("cost"));
        assert!(!json.contains("timestamp"));
        assert!(!json.contains("cache_hint"));
    }

    #[test]
    fn canonical_size_matches_json_len() {
        let msgs = vec![Message::user("hello"), Message::assistant("world")];
        let fp = compute_input_fingerprint(&msgs);
        assert_eq!(fp.canonical_size, canonical_json(&msgs).len());
    }

    #[test]
    fn all_roles_serialize() {
        for &role in &[Role::System, Role::User, Role::Assistant, Role::Tool] {
            let m = Message {
                role,
                content: "x".into(),
                cache_hint: crate::model::CacheHint::None,
                extra: crate::model::MessageExtra::default(),
            };
            let json = canonical_json(&[m]);
            assert!(json.contains("\"role\""), "role field missing for {role:?}");
        }
    }
}
