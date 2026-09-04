use axum::http::StatusCode;
use axum::response::IntoResponse;
use sha2::{Digest, Sha256};

use crate::authz::{load_and_activate_tenant_policies, persist_and_activate_policy};
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

pub(super) async fn existing_enabled_rule_id(
    state: &ServerState,
    tenant: &str,
    rule: &str,
) -> Option<String> {
    let store = state.policy_store()?;
    let rows = store.load_policies_for_tenant(tenant).await.ok()?;
    rows.into_iter()
        .find(|row| row.enabled && row.cedar_text.trim() == rule.trim())
        .map(|row| row.policy_id)
}

pub(super) fn rule_added_json(tenant: &str, policy_id: &str) -> axum::response::Response {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "tenant": tenant,
            "policy_id": policy_id,
            "status": "rule_added",
        })),
    )
        .into_response()
}

pub(super) async fn persist_new_rule(
    state: &ServerState,
    tenant: &str,
    rule: &str,
    created_by: &str,
) -> axum::response::Response {
    if let Some(existing_id) = existing_enabled_rule_id(state, tenant, rule).await {
        return rule_added_json(tenant, &existing_id);
    }
    let policy_id = add_rule_policy_id(rule);
    debug_assert_ne!(policy_id, "primary");
    debug_assert!(!policy_id.is_empty());
    let prospective = build_prospective_enabled_text(state, tenant, Some((&policy_id, rule))).await;
    if let Err(resp) = crate::api::validate_and_reload_policies(state, tenant, &prospective) {
        return resp;
    }
    persist_and_activate_policy(state, tenant, &policy_id, rule, created_by).await;
    {
        let mut policies = state.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        policies.insert(tenant.to_string(), prospective);
    }
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Policies);
    rule_added_json(tenant, &policy_id)
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
