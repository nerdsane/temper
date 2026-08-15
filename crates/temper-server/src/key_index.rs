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
/// Null is a distinct, indexable key value (e.g. a filesystem root's null
/// `ParentId`), tagged so it can never collide with the empty string.
const TAG_NULL: u8 = b'0';

/// The stable signature of a type's declared key-set: the sorted, comma-joined key
/// NAMES. Recorded in the key-index backfill watermark and compared on read, so that
/// declaring an ADDITIONAL key (a changed signature) re-keys the type instead of being
/// treated as already complete, and a keyed read only trusts absence once the full
/// current key-set is backfilled (ARN-68). Deterministic (sorted, no map iteration).
pub fn declared_key_set_signature(keys: &[DeclaredKey]) -> String {
    let mut names: Vec<&str> = keys.iter().map(|key| key.name.as_str()).collect();
    names.sort();
    names.join(",")
}

/// Canonical `key_hash` for a declared key's values, or `None` when the key is
/// not fully present.
///
/// `properties` is the declared key's property set **in declared order** (the
/// order is part of the canonical form). For each property the entity's current
/// scalar value is encoded as `(type_tag, canonical_text)`. A `null` value — or a
/// property **absent** from `fields` (e.g. a filesystem root has no `ParentId`
/// field at all) — is a valid, indexable key component encoded under `TAG_NULL`,
/// matching OData's `prop eq null` semantics. Returns `None` if the key has no
/// properties, if a **present** property is non-scalar (array/object), or if
/// **every** component is null/absent (an all-null key has no distinguishing value
/// and would collide for every such entity).
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
    // A declared-key property that is ABSENT from the entity's fields is treated
    // as null — consistent with OData, where `prop eq null` matches a missing
    // field. This is what keys a filesystem root directory: it is created with no
    // `ParentId` field at all (Directory has no ParentId state var), and the read
    // looks it up by `ParentId eq null`. Absent and explicit-null both index under
    // TAG_NULL so the keyed read resolves the root instead of scanning.
    //
    // But an entity with EVERY key component null/absent has no distinguishing key
    // value, so it is NOT indexed — otherwise all such entities would collide on a
    // single all-null hash and trip the uniqueness constraint. The root is safe: it
    // has `Name`/`WorkspaceId` present, only `ParentId` is null.
    let null = serde_json::Value::Null;
    let mut any_present = false;
    for prop in properties {
        let value = match lookup_field(fields, prop) {
            Some(v) if !v.is_null() => {
                any_present = true;
                v
            }
            other => other.unwrap_or(&null),
        };
        let (tag, canon) = canonical_value(value)?;
        hasher.update([tag]);
        hasher.update(canon.as_bytes());
        hasher.update([RECORD_SEP]);
    }
    if !any_present {
        return None;
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
        // Null is a real, distinct key value (a root's null `ParentId`), indexed
        // under its own tag so it never collides with the empty string.
        Value::Null => Some((TAG_NULL, String::new())),
        Value::Bool(b) => Some((TAG_BOOL, if *b { "1" } else { "0" }.to_string())),
        Value::Number(n) => Some((TAG_NUMBER, n.to_string())),
        Value::String(s) => Some((TAG_STRING, s.clone())),
        // Arrays/objects are not scalar key components.
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// Resolve a read's equality predicates to a declared key + its `key_hash`, for
/// the read-plane fast path (ADR-0153): if `equality_pairs` is exactly one
/// declared key's property set, return `(key_name, key_hash)` so the caller can
/// probe `entity_key_index` instead of scanning. `None` if no declared key
/// matches.
///
/// Values are the **typed** literals from the `$filter` (string, number, bool,
/// or `null`), so the read-side hash matches the write-side hash component-for-
/// component — including a `null` key value (a filesystem root's null `ParentId`),
/// which is what makes the root directory lookup keyed instead of a scan. A
/// type/value mismatch only declines the fast path (caller falls back to the
/// scan), never a wrong hit.
pub fn resolve_query_to_key(
    keys: &[DeclaredKey],
    equality_pairs: &[(String, serde_json::Value)],
) -> Option<(String, String)> {
    let mut fields = serde_json::Map::new();
    for (prop, value) in equality_pairs {
        fields.insert(prop.clone(), value.clone());
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
    fn absent_and_null_key_components_are_indexable_as_null() {
        let props = vec!["WorkspaceId".to_string(), "Path".to_string()];
        // ABSENT property (not in fields at all) -> indexed as null...
        let missing = fields(&[("WorkspaceId", json!("ws1"))]);
        // ...and equal to an explicit-null value for the same property.
        let null_path = fields(&[("WorkspaceId", json!("ws1")), ("Path", json!(null))]);
        assert_eq!(
            canonical_key_hash("path", &props, &missing),
            canonical_key_hash("path", &props, &null_path),
            "absent and explicit-null must hash identically (OData eq-null semantics)"
        );
        assert!(canonical_key_hash("path", &props, &missing).is_some());
        // A present non-empty value must differ from null/absent.
        let present = fields(&[("WorkspaceId", json!("ws1")), ("Path", json!("/a.md"))]);
        assert_ne!(
            canonical_key_hash("path", &props, &present),
            canonical_key_hash("path", &props, &missing)
        );
        // no declared properties -> nothing to index.
        let f = fields(&[("WorkspaceId", json!("ws1"))]);
        assert!(canonical_key_hash("path", &[], &f).is_none());
    }

    #[test]
    fn all_null_or_absent_key_is_not_indexed() {
        // An entity with NO key values set (e.g. an Order created with `{}`) must
        // not be keyed — else every such entity collides on a single all-null hash
        // and the second co-commit trips the uniqueness constraint.
        let props = vec!["WorkspaceId".to_string(), "Path".to_string()];
        assert!(canonical_key_hash("path", &props, &fields(&[])).is_none());
        let all_null = fields(&[("WorkspaceId", json!(null)), ("Path", json!(null))]);
        assert!(canonical_key_hash("path", &props, &all_null).is_none());
        // But one present component is enough to key it (the root's case).
        let one_present = fields(&[("WorkspaceId", json!("ws1"))]);
        assert!(canonical_key_hash("path", &props, &one_present).is_some());
    }

    #[test]
    fn root_directory_absent_parentid_keys_and_matches_eq_null_read() {
        // The real bug (ARN-68): the FS root Directory is created with no ParentId
        // field at all (Directory has no ParentId state var). The WRITE/backfill
        // hashes its actual fields {Name, WorkspaceId} (ParentId absent); the READ
        // looks it up by `... and ParentId eq null`. Both must agree.
        let key = vec![
            "Name".to_string(),
            "WorkspaceId".to_string(),
            "ParentId".to_string(),
        ];
        // Write side: root entity fields — ParentId genuinely absent.
        let root_fields = fields(&[("Name", json!("/")), ("WorkspaceId", json!("katagami"))]);
        let write_hash =
            canonical_key_hash("name_parent", &key, &root_fields).expect("absent ParentId indexes");

        // Read side: `Name eq '/' and WorkspaceId eq 'katagami' and ParentId eq null`.
        let decl = vec![DeclaredKey {
            name: "name_parent".to_string(),
            properties: key.clone(),
            entity_id: false,
        }];
        let read_pairs = vec![
            ("Name".to_string(), json!("/")),
            ("WorkspaceId".to_string(), json!("katagami")),
            ("ParentId".to_string(), serde_json::Value::Null),
        ];
        let (_, read_hash) = resolve_query_to_key(&decl, &read_pairs).expect("root read resolves");
        assert_eq!(
            write_hash, read_hash,
            "absent-ParentId write must equal eq-null read — else the root lookup scans (413)"
        );
    }

    fn path_key() -> Vec<DeclaredKey> {
        vec![DeclaredKey {
            name: "path".to_string(),
            properties: vec!["WorkspaceId".to_string(), "Path".to_string()],
            entity_id: false,
        }]
    }

    #[test]
    fn resolve_query_matches_declared_key_and_agrees_with_write_hash() {
        let pairs = vec![
            ("WorkspaceId".to_string(), json!("ws1")),
            ("Path".to_string(), json!("/a.md")),
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
            ("WorkspaceId".to_string(), json!("ws-1")),
            ("Path".to_string(), json!("/a.md")),
        ];
        let decl = vec![DeclaredKey {
            name: "ws_path".to_string(),
            properties: key.clone(),
            entity_id: false,
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
            resolve_query_to_key(&path_key(), &[("WorkspaceId".into(), json!("ws1"))]).is_none()
        );
        assert!(
            resolve_query_to_key(
                &path_key(),
                &[
                    ("Other".into(), json!("x")),
                    ("Path".into(), json!("/a.md"))
                ]
            )
            .is_none()
        );
        assert!(resolve_query_to_key(&[], &[("WorkspaceId".into(), json!("ws1"))]).is_none());
    }

    #[test]
    fn null_key_component_is_indexable_and_read_matches_write() {
        // The filesystem root directory case (ARN-68): the root has a null ParentId.
        // Write side: hash the entity's (Name='/', WorkspaceId, ParentId=null).
        let key = vec![
            "Name".to_string(),
            "WorkspaceId".to_string(),
            "ParentId".to_string(),
        ];
        let write_fields = fields(&[
            ("Name", json!("/")),
            ("WorkspaceId", json!("katagami")),
            ("ParentId", json!(null)),
        ]);
        let write_hash =
            canonical_key_hash("name_parent", &key, &write_fields).expect("null is indexable");

        // Read side: `Name eq '/' and WorkspaceId eq 'katagami' and ParentId eq null`
        // resolves to the SAME hash — so the root lookup is keyed, not a scan.
        let decl = vec![DeclaredKey {
            name: "name_parent".to_string(),
            properties: key.clone(),
            entity_id: false,
        }];
        let read_pairs = vec![
            ("Name".to_string(), json!("/")),
            ("WorkspaceId".to_string(), json!("katagami")),
            ("ParentId".to_string(), json!(null)),
        ];
        let (_, read_hash) = resolve_query_to_key(&decl, &read_pairs).expect("null read resolves");
        assert_eq!(
            write_hash, read_hash,
            "null ParentId must hash equal on write and read — else the root lookup misses and scans (413)"
        );

        // A null component must NOT collide with the empty string.
        let empty_fields = fields(&[
            ("Name", json!("/")),
            ("WorkspaceId", json!("katagami")),
            ("ParentId", json!("")),
        ]);
        assert_ne!(
            canonical_key_hash("name_parent", &key, &empty_fields).unwrap(),
            write_hash,
            "null and empty-string must be distinct key values"
        );
    }
}
