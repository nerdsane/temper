use sha2::{Digest, Sha256};

use crate::authz::load_and_activate_tenant_policies;
use crate::state::ServerState;
use crate::storage::PolicyStoreRow;

/// Stable id for a rule added via POST `/policies/rules`.
///
/// Hashing the Cedar text makes a second add of the same rule land on the
/// same row instead of growing `primary` (ARN-462 / ARN-286).
pub(super) fn add_rule_policy_id(cedar_text: &str) -> String {
    let digest = Sha256::digest(cedar_text.as_bytes());
    format!("rule:{digest:x}")
}

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

pub(super) async fn reload_tenant_from_store(state: &ServerState, tenant: &str) {
    load_and_activate_tenant_policies(state, tenant).await;
}

pub(super) async fn build_prospective_enabled_text(
    state: &ServerState,
    tenant: &str,
    additional: Option<(&str, &str)>,
) -> String {
    let mut text = {
        let policies = state.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        policies.get(tenant).cloned().unwrap_or_default()
    };
    if let Some((_id, cedar_text)) = additional {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(cedar_text);
    }
    text
}

pub(super) async fn build_prospective_enabled_text_with_override(
    state: &ServerState,
    tenant: &str,
    override_policy_id: &str,
    override_cedar_text: &str,
    override_enabled: Option<bool>,
) -> String {
    let rows = if let Some(store) = state.policy_store() {
        store
            .load_policies_for_tenant(tenant)
            .await
            .unwrap_or_default()
    } else {
        vec![]
    };

    let mut combined = String::new();
    for row in &rows {
        let is_target = row.policy_id == override_policy_id;
        let cedar_text = if is_target {
            override_cedar_text
        } else {
            &row.cedar_text
        };
        let enabled = if is_target {
            override_enabled.unwrap_or(row.enabled)
        } else {
            row.enabled
        };
        if !enabled {
            continue;
        }
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(cedar_text);
    }
    combined
}
