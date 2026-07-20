//! Entity field index — EAV table for OData filter push-down.
//!
//! Maintains an Entity-Attribute-Value index of top-level scalar fields
//! so that OData `$filter` expressions can be translated to SQL WHERE
//! clauses. This avoids materializing every actor in a collection query
//! just to evaluate filters in memory.

use libsql::{TransactionBehavior, Value, params, params_from_iter};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use temper_runtime::persistence::{PersistenceError, storage_error};
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use super::write_gate::WritePriority;
use super::{TursoEventStore, TursoQueryProjectionRow};

mod queries;
mod upserts;

/// Match the Postgres query-plane limit: large scalar values stay in
/// `entity_catalog.fields` but are not copied into the filter index.
const MAX_INDEXABLE_FIELD_VALUE_BYTES: usize = 2000;

fn projection_hash(status: &str, fields: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(status.as_bytes());
    hasher.update(b"\n");

    if let Some(obj) = fields.as_object() {
        for (field_name, value) in obj {
            let field_value = scalar_to_text(value);
            if field_value.is_none() && !value.is_null() {
                continue;
            }
            hasher.update(field_name.as_bytes());
            hasher.update(b"=");
            match field_value {
                Some(field_value) => hasher.update(field_value.as_bytes()),
                None => hasher.update(b"<null>"),
            }
            hasher.update(b"\n");
        }
    }

    format!("{:x}", hasher.finalize())
}

fn canonical_projection_status<'a>(fallback: &'a str, state: &'a serde_json::Value) -> &'a str {
    state
        .get("status")
        .and_then(|value| value.as_str())
        .filter(|status| !status.is_empty())
        .unwrap_or(fallback)
}

/// Sparse projected field values loaded from the durable query plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedEntityFieldsRow {
    pub entity_id: String,
    pub status: String,
    pub fields: BTreeMap<String, Option<String>>,
}

/// One row from `entity_catalog`, with the full JSON fields blob preserved.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityCatalogRow {
    pub entity_id: String,
    pub status: String,
    pub fields: serde_json::Value,
    pub state: Option<serde_json::Value>,
    pub sequence_nr: u64,
}

/// One durable projection upsert in a batch.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryProjectionUpsert {
    pub entity_type: String,
    pub entity_id: String,
    pub status: String,
    pub fields: serde_json::Value,
    pub state: serde_json::Value,
    pub indexed_fields: serde_json::Value,
    pub sequence_nr: u64,
    pub known_new: bool,
}

/// Convert a JSON scalar to a TEXT representation for the index.
///
/// Returns `None` for non-scalar types (objects, arrays) — these are not indexed.
/// `null` values return `None` (stored as SQL NULL in field_value).
fn scalar_to_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => None,
    }
}

fn indexed_projection_fields(
    status: &str,
    fields: &serde_json::Value,
) -> Vec<(String, Option<String>)> {
    let mut indexed_fields = Vec::new();

    if let Some(obj) = fields.as_object() {
        for (field_name, value) in obj {
            if field_name == "Status" {
                continue;
            }

            let field_value = scalar_to_text(value);
            if field_value.is_none() && !value.is_null() {
                continue;
            }
            if field_value
                .as_ref()
                .is_some_and(|value| value.len() > MAX_INDEXABLE_FIELD_VALUE_BYTES)
            {
                continue;
            }

            indexed_fields.push((field_name.clone(), field_value));
        }
    }

    // Also index the status as a pseudo-field so `$filter=Status eq 'Active'` works.
    indexed_fields.push(("Status".to_string(), Some(status.to_string())));
    indexed_fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_to_text_converts_primitives() {
        assert_eq!(
            scalar_to_text(&serde_json::json!("hello")),
            Some("hello".to_string())
        );
        assert_eq!(
            scalar_to_text(&serde_json::json!(42)),
            Some("42".to_string())
        );
        assert_eq!(
            scalar_to_text(&serde_json::json!(true)),
            Some("true".to_string())
        );
        assert_eq!(scalar_to_text(&serde_json::Value::Null), None);
    }

    #[test]
    fn scalar_to_text_skips_complex_types() {
        assert_eq!(scalar_to_text(&serde_json::json!({"a": 1})), None);
        assert_eq!(scalar_to_text(&serde_json::json!([1, 2, 3])), None);
    }

    #[test]
    fn indexed_projection_fields_skips_oversized_scalars() {
        let long = "x".repeat(MAX_INDEXABLE_FIELD_VALUE_BYTES + 1);
        let fields = serde_json::json!({
            "Title": "short",
            "Payload": long,
        });

        let indexed = indexed_projection_fields("Active", &fields);

        assert!(
            indexed
                .iter()
                .any(|(name, value)| name == "Title" && value.as_deref() == Some("short"))
        );
        assert!(indexed.iter().all(|(name, _)| name != "Payload"));
        assert!(
            indexed
                .iter()
                .any(|(name, value)| name == "Status" && value.as_deref() == Some("Active"))
        );
    }
}
