//! Declared composite-key index hashing (ADR-0153, ARN-68).
//!
//! A declared `[[key]]` (an alternate / unique key) is reduced to a single
//! canonical, type-tagged `key_hash` so the kernel can maintain `entity_key_index`
//! and answer "present -> entity_id" or "absent" in one `O(log n)` probe — the
//! negative-existence access path the read plane lacks today.
//!
//! Both the write path (the entity actor, hashing the new state's key values) and
//! the read path (resolving a `$filter` that matches a declared key) compute the
//! hash here, so they always agree. The function is **deterministic** (SHA-256, no
//! clock, no randomness, no map iteration) and therefore safe under deterministic
//! simulation.

use sha2::{Digest, Sha256};

/// Separates `key_name` from the value list.
const UNIT_SEP: u8 = 0x1F;
/// Separates one property's encoded value from the next.
const RECORD_SEP: u8 = 0x1E;

/// Type tags keep `"5"` (string) and `5` (number) from colliding.
const TAG_STRING: u8 = b'S';
const TAG_NUMBER: u8 = b'N';
const TAG_BOOL: u8 = b'B';

/// Canonical `key_hash` for a declared key's values, or `None` when the key is
/// not fully present.
///
/// `properties` is the declared key's property set **in declared order** (the
/// order is part of the canonical form). For each property the entity's current
/// scalar value is encoded as `(type_tag, canonical_text)`. Returns `None` if the
/// key has no properties, or if any property is missing, null, or non-scalar — a
/// partial key is not indexable, so the entity simply has no `entity_key_index`
/// row for that key (it is still reachable by `Id`).
pub fn canonical_key_hash(
    key_name: &str,
    properties: &[String],
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    if properties.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(key_name.as_bytes());
    hasher.update([UNIT_SEP]);
    for prop in properties {
        let (tag, canon) = canonical_value(fields.get(prop)?)?;
        hasher.update([tag]);
        hasher.update(canon.as_bytes());
        hasher.update([RECORD_SEP]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

/// Encode one scalar value as `(type_tag, canonical_text)`. `None` for null or a
/// non-scalar (a partial / unindexable key component).
fn canonical_value(value: &serde_json::Value) -> Option<(u8, String)> {
    use serde_json::Value;
    match value {
        Value::Null => None,
        Value::Bool(b) => Some((TAG_BOOL, if *b { "1" } else { "0" }.to_string())),
        Value::Number(n) => Some((TAG_NUMBER, n.to_string())),
        Value::String(s) => Some((TAG_STRING, s.clone())),
        Value::Array(_) | Value::Object(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn same_values_hash_equal_different_values_differ() {
        let props = vec!["WorkspaceId".to_string(), "Path".to_string()];
        let a = fields(&[("WorkspaceId", json!("ws1")), ("Path", json!("/a.md"))]);
        let b = fields(&[("WorkspaceId", json!("ws1")), ("Path", json!("/a.md"))]);
        let c = fields(&[("WorkspaceId", json!("ws1")), ("Path", json!("/b.md"))]);
        assert_eq!(
            canonical_key_hash("path", &props, &a),
            canonical_key_hash("path", &props, &b)
        );
        assert_ne!(
            canonical_key_hash("path", &props, &a),
            canonical_key_hash("path", &props, &c)
        );
    }

    #[test]
    fn type_tag_prevents_string_number_collision() {
        let props = vec!["K".to_string()];
        let as_str = fields(&[("K", json!("5"))]);
        let as_num = fields(&[("K", json!(5))]);
        assert_ne!(
            canonical_key_hash("k", &props, &as_str),
            canonical_key_hash("k", &props, &as_num)
        );
    }

    #[test]
    fn key_name_is_part_of_the_hash() {
        let props = vec!["K".to_string()];
        let f = fields(&[("K", json!("v"))]);
        assert_ne!(
            canonical_key_hash("a", &props, &f),
            canonical_key_hash("b", &props, &f)
        );
    }

    #[test]
    fn property_order_matters() {
        let f = fields(&[("X", json!("1")), ("Y", json!("2"))]);
        let xy = vec!["X".to_string(), "Y".to_string()];
        let yx = vec!["Y".to_string(), "X".to_string()];
        assert_ne!(
            canonical_key_hash("k", &xy, &f),
            canonical_key_hash("k", &yx, &f)
        );
    }

    #[test]
    fn missing_or_null_or_empty_yields_none() {
        let props = vec!["WorkspaceId".to_string(), "Path".to_string()];
        // missing Path
        let missing = fields(&[("WorkspaceId", json!("ws1"))]);
        assert!(canonical_key_hash("path", &props, &missing).is_none());
        // null Path
        let null_path = fields(&[("WorkspaceId", json!("ws1")), ("Path", json!(null))]);
        assert!(canonical_key_hash("path", &props, &null_path).is_none());
        // no declared properties
        let f = fields(&[("WorkspaceId", json!("ws1"))]);
        assert!(canonical_key_hash("path", &[], &f).is_none());
    }
}
