use axum::http::StatusCode;
use axum::response::IntoResponse;
use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use temper_authz::statements::{remove_statements, statement_keys};

use crate::authz::{load_and_activate_tenant_policies, record_policy_change};
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
    let prospective = match live_text_with_entry(state, tenant, &policy_id, rule).await {
        Ok(Some(text)) => text,
        Ok(None) => return rule_added_json(tenant, &policy_id),
        Err(refused) => return refused.into_response(),
    };
    if let Err(resp) =
        persist_and_reload_entry(state, tenant, &policy_id, rule, created_by, prospective).await
    {
        return resp;
    }
    rule_added_json(tenant, &policy_id)
}

/// Validate without changing the live engine, save successfully, then activate.
/// A failed save leaves both the serving engine and its text cache untouched.
pub(super) async fn persist_and_reload_entry(
    state: &ServerState,
    tenant: &str,
    policy_id: &str,
    cedar_text: &str,
    created_by: &str,
    prospective: String,
) -> Result<(), axum::response::Response> {
    if let Err(error) = state.authz.validate_tenant_policies(&prospective) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Policy validation failed: {error}"),
        )
            .into_response());
    }
    let changed = if let Some(store) = state.policy_store() {
        match store
            .save_policy(tenant, policy_id, cedar_text, created_by)
            .await
        {
            Ok(changed) => changed,
            Err(error) => {
                tracing::warn!(%error, tenant, policy_id, "failed to store policy entry");
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Policy write failed; live policies unchanged",
                )
                    .into_response());
            }
        }
    } else {
        false
    };
    if let Err(error) = state.authz.reload_tenant_policies(tenant, &prospective) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Policy saved but activation failed: {error}"),
        )
            .into_response());
    }
    {
        let mut policies = state.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        policies.insert(tenant.to_string(), prospective);
    }
    if changed {
        record_policy_change(state, tenant, policy_id, created_by);
    }
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Policies);
    Ok(())
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

/// The tenant's live Cedar once the row `policy_id` holds `cedar_text`.
///
/// What the row held before leaves the live text, except a statement another
/// enabled row also holds, and so does every live copy of the posted
/// statements; then the posted text is added once. Posting the same entry
/// again leaves the live text as it was, and posting new text under an id
/// leaves none of its old statements live. An existing disabled entry stays
/// disabled, matching `save_policy`: its posted text is stored but not activated.
/// The live text now agrees with what a restart loads from the rows.
///
/// `None` when the row already holds exactly `cedar_text` and all of it is
/// live: nothing changes, so the caller reloads nothing.
///
/// # Errors
///
/// The status and reason to answer with when the posted text or the live
/// text is not Cedar (400), or the tenant's rows cannot be read (500).
pub(in crate::api) async fn live_text_with_entry(
    state: &ServerState,
    tenant: &str,
    policy_id: &str,
    cedar_text: &str,
) -> Result<Option<String>, (StatusCode, String)> {
    let refuse = |status: StatusCode, why: String| Err((status, why));
    let posted = match statement_keys(cedar_text) {
        Ok(keys) => keys,
        Err(error) => {
            return refuse(
                StatusCode::BAD_REQUEST,
                format!("Policy validation failed: {error}"),
            );
        }
    };
    let mut leaving = posted.clone();
    let mut unchanged = false;
    let mut enabled = true;
    if let Some(store) = state.policy_store() {
        let rows = match store.load_policies_for_tenant(tenant).await {
            Ok(rows) => rows,
            Err(error) => {
                return refuse(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to read the tenant's policy rows: {error}"),
                );
            }
        };
        let mut previous = BTreeSet::new();
        let mut held_by_others = BTreeSet::new();
        for row in rows
            .iter()
            .filter(|row| row.enabled || row.policy_id == policy_id)
        {
            if row.policy_id == policy_id {
                enabled = row.enabled;
                unchanged = row.enabled && row.cedar_text == cedar_text;
            }
            // A row that does not parse holds no live statement: boot cannot load it.
            let Ok(keys) = statement_keys(&row.cedar_text) else {
                continue;
            };
            if row.policy_id == policy_id {
                previous.extend(keys);
            } else {
                held_by_others.extend(keys);
            }
        }
        if !enabled {
            // Do not remove a posted statement that another enabled row holds:
            // this disabled entry will not add it back.
            leaving.retain(|key| !held_by_others.contains(key));
        }
        leaving.extend(previous.difference(&held_by_others).cloned());
    }
    let live = {
        let policies = state.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        policies.get(tenant).cloned().unwrap_or_default()
    };
    if unchanged && statement_keys(&live).is_ok_and(|live_keys| posted.is_subset(&live_keys)) {
        return Ok(None);
    }
    let kept = match remove_statements(&live, &leaving) {
        Ok(kept) => kept,
        Err(error) => {
            return refuse(
                StatusCode::BAD_REQUEST,
                format!(
                    "Policy validation failed: the tenant's live Cedar does not parse: {error}"
                ),
            );
        }
    };
    let kept = kept.trim_end();
    Ok(Some(if !enabled {
        kept.to_string()
    } else if kept.is_empty() {
        cedar_text.to_string()
    } else {
        format!("{kept}\n{cedar_text}")
    }))
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
