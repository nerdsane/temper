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
use temper_jit::table::types::DeclaredKey;

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
        let (tag, canon) = canonical_value(lookup_field(fields, prop)?)?;
        hasher.update([tag]);
        hasher.update(canon.as_bytes());
        hasher.update([RECORD_SEP]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

/// Look up a declared key property in the entity's fields, tolerant of the
/// snake/Pascal case split between how params are stored and how OData filters
/// name them. Entity writes store action params verbatim (often snake_case, e.g.
/// `workspace_id`), while OData reads name properties in the CSDL's PascalCase
/// (`WorkspaceId`) — and some callers write Pascal directly. The declared
/// `[[key]]` uses one convention; this finds the field regardless. Exact match
/// first (so existing same-case behavior is unchanged), then snake, then Pascal.
/// The hash itself is over key values, not names, so a case-tolerant lookup makes
/// the write-side and read-side hashes agree for the same entity.
fn lookup_field<'a>(
    fields: &'a serde_json::Map<String, serde_json::Value>,
    prop: &str,
) -> Option<&'a serde_json::Value> {
    if let Some(v) = fields.get(prop) {
        return Some(v);
    }
    let snake = temper_spec::to_snake_case(prop);
    if let Some(v) = fields.get(&snake) {
        return Some(v);
    }
    let pascal = temper_spec::to_pascal_case(prop);
    fields.get(&pascal)
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

/// Resolve a read's equality predicates to a declared key + its `key_hash`, for
/// the read-plane fast path (ADR-0153): if `equality_pairs` is exactly one
/// declared key's property set, return `(key_name, key_hash)` so the caller can
/// probe `entity_key_index` instead of scanning. `None` if no declared key
/// matches.
///
/// Values are taken as strings (how the OData URL / `$filter` deliver them),
/// which matches the write-side hash for string-typed key properties — the case
/// for every declared business key today (File `WorkspaceId+Path`, etc.). A
/// non-string key property would need the value coerced to its declared type
/// before this matches; that is tracked as a follow-up and a mismatch only
/// declines the fast path (the caller falls back to the scan), never a wrong hit.
pub fn resolve_query_to_key(
    keys: &[DeclaredKey],
    equality_pairs: &[(String, String)],
) -> Option<(String, String)> {
    let mut fields = serde_json::Map::new();
    for (prop, value) in equality_pairs {
        fields.insert(prop.clone(), serde_json::Value::String(value.clone()));
    }
    for key in keys {
        if key.properties.len() == equality_pairs.len()
            && key.properties.iter().all(|p| fields.contains_key(p))
        {
            let hash = canonical_key_hash(&key.name, &key.properties, &fields)?;
            return Some((key.name.clone(), hash));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
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

    fn path_key() -> Vec<DeclaredKey> {
        vec![DeclaredKey {
            name: "path".to_string(),
            properties: vec!["WorkspaceId".to_string(), "Path".to_string()],
        }]
    }

    #[test]
    fn resolve_query_matches_declared_key_and_agrees_with_write_hash() {
        let pairs = vec![
            ("WorkspaceId".to_string(), "ws1".to_string()),
            ("Path".to_string(), "/a.md".to_string()),
        ];
        let (name, hash) = resolve_query_to_key(&path_key(), &pairs).expect("matches declared key");
        assert_eq!(name, "path");
        // The read-side hash MUST equal the write-side hash for the same string
        // values — that is why present/absent works.
        let write_side = canonical_key_hash(
            "path",
            &["WorkspaceId".to_string(), "Path".to_string()],
            &fields(&[("WorkspaceId", json!("ws1")), ("Path", json!("/a.md"))]),
        )
        .unwrap();
        assert_eq!(
            hash, write_side,
            "read-side hash must equal write-side hash"
        );
    }

    #[test]
    fn write_snake_and_read_pascal_hash_agree() {
        // The real-entity case: the write stores params snake_case (workspace_id,
        // path), the OData read names them PascalCase (WorkspaceId, Path). The hash
        // is over VALUES, and the lookup is case-tolerant, so both sides agree.
        let key = vec!["WorkspaceId".to_string(), "Path".to_string()];
        let write_fields = fields(&[
            ("workspace_id", json!("ws-1")),
            ("path", json!("/a.md")),
            ("Status", json!("Ready")),
        ]);
        let write_hash = canonical_key_hash("ws_path", &key, &write_fields).expect("write hash");

        // Read side: resolve a PascalCase $filter against the same declared key.
        let read_pairs = vec![
            ("WorkspaceId".to_string(), "ws-1".to_string()),
            ("Path".to_string(), "/a.md".to_string()),
        ];
        let decl = vec![DeclaredKey {
            name: "ws_path".to_string(),
            properties: key.clone(),
        }];
        let (_, read_hash) = resolve_query_to_key(&decl, &read_pairs).expect("read resolves");
        assert_eq!(
            write_hash, read_hash,
            "write (snake fields) and read (Pascal filter) must hash equal — else the keyed read always misses"
        );

        // And Pascal-stored writes (other callers) also agree.
        let pascal_write = fields(&[("WorkspaceId", json!("ws-1")), ("Path", json!("/a.md"))]);
        assert_eq!(
            canonical_key_hash("ws_path", &key, &pascal_write).expect("pascal write hash"),
            read_hash,
        );
    }

    #[test]
    fn resolve_query_declines_when_no_key_matches() {
        // wrong arity, wrong property, and no declared keys -> decline (fall back to scan)
        assert!(
            resolve_query_to_key(&path_key(), &[("WorkspaceId".into(), "ws1".into())]).is_none()
        );
        assert!(
            resolve_query_to_key(
                &path_key(),
                &[
                    ("Other".into(), "x".into()),
                    ("Path".into(), "/a.md".into())
                ]
            )
            .is_none()
        );
        assert!(resolve_query_to_key(&[], &[("WorkspaceId".into(), "ws1".into())]).is_none());
    }
}
