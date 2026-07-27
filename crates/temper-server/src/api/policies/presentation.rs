//! Stable policy API response projection.

use crate::storage::PolicyStoreRow;

fn policy_source(policy_id: &str) -> &'static str {
    if policy_id.starts_with("os-app:") {
        "os-app"
    } else if policy_id.starts_with("decision:") {
        "decision"
    } else if policy_id == "migrated-legacy" {
        "migrated-legacy"
    } else {
        "manual"
    }
}

/// Serialize a durable policy row to the public API shape.
pub(super) fn policy_row_to_json(row: &PolicyStoreRow) -> serde_json::Value {
    serde_json::json!({
        "tenant": row.tenant,
        "policy_id": row.policy_id,
        "cedar_text": row.cedar_text,
        "enabled": row.enabled,
        "policy_hash": row.policy_hash,
        "created_at": row.created_at,
        "created_by": row.created_by,
        "source": policy_source(&row.policy_id),
    })
}
