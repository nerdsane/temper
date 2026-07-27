//! Policy management API endpoints.
//!
//! Handles Cedar policy CRUD operations for tenants, including full replacement,
//! incremental rule addition, individual policy listing/toggling/editing/deletion,
//! and cross-tenant policy views.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use super::PolicyAuthed;
use crate::state::ServerState;
use crate::storage::PolicyStoreRow;

mod mutation;
mod presentation;
use mutation::{PolicyMutationError, mutate_tenant_policies};
use presentation::policy_row_to_json;

fn is_decision_policy(policy_id: &str) -> bool {
    policy_id.starts_with("decision:")
}

fn mutation_error_response(error: PolicyMutationError) -> Response {
    match error {
        PolicyMutationError::NotFound => {
            (StatusCode::NOT_FOUND, "Policy not found").into_response()
        }
        PolicyMutationError::AlreadyExists => {
            (StatusCode::CONFLICT, "Policy already exists").into_response()
        }
        PolicyMutationError::Invalid(error) => (
            StatusCode::BAD_REQUEST,
            format!("Policy validation failed: {error}"),
        )
            .into_response(),
        PolicyMutationError::Contended => (
            StatusCode::CONFLICT,
            "Policy set changed concurrently; retry the request",
        )
            .into_response(),
        PolicyMutationError::Unavailable(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Policy publication failed: {error}"),
        )
            .into_response(),
    }
}

/// GET /api/tenants/{tenant}/policies — return current Cedar policy text.
///
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/policies"))]
pub(crate) async fn handle_get_policies(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    _auth: PolicyAuthed,
) -> impl IntoResponse {
    let policies = state.tenant_policies.read().unwrap(); // ci-ok: infallible lock
    let text = policies.get(&tenant).cloned().unwrap_or_default();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"tenant": tenant, "policy_text": text})),
    )
        .into_response()
}

/// PUT /api/tenants/{tenant}/policies — replace all policies (validate then reload).
///
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, otel.name = "PUT /api/tenants/{tenant}/policies"))]
pub(crate) async fn handle_put_policies(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    _auth: PolicyAuthed,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let body_json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "invalid JSON in put policies request");
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")).into_response();
        }
    };

    let policy_text = match body_json.get("policy_text").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => {
            tracing::warn!("missing 'policy_text' field in put policies request");
            return (
                StatusCode::BAD_REQUEST,
                "Missing 'policy_text' field in request body",
            )
                .into_response();
        }
    };

    let created_at = sim_now().to_rfc3339();
    if let Err(error) = mutate_tenant_policies(&state, &tenant, |rows| {
        rows.clear();
        if !policy_text.is_empty() {
            rows.push(PolicyStoreRow {
                tenant: tenant.clone(),
                policy_id: "primary".to_string(),
                cedar_text: policy_text.clone(),
                policy_hash: String::new(),
                created_at: created_at.clone(),
                created_by: "api".to_string(),
                enabled: true,
            });
        }
        Ok(())
    })
    .await
    {
        return mutation_error_response(error);
    }

    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Policies);

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"tenant": tenant, "status": "loaded"})),
    )
        .into_response()
}

/// POST /api/tenants/{tenant}/policies/rules — append a single rule.
///
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, otel.name = "POST /api/tenants/{tenant}/policies/rules"))]
pub(crate) async fn handle_add_policy_rule(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    _auth: PolicyAuthed,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let body_json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "invalid JSON in add policy rule request");
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")).into_response();
        }
    };

    let rule = match body_json.get("rule").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => {
            tracing::warn!("missing 'rule' field in add policy rule request");
            return (
                StatusCode::BAD_REQUEST,
                "Missing 'rule' field in request body",
            )
                .into_response();
        }
    };

    let created_at = sim_now().to_rfc3339();
    if let Err(error) = mutate_tenant_policies(&state, &tenant, |rows| {
        if let Some(primary) = rows.iter_mut().find(|row| row.policy_id == "primary") {
            if primary.cedar_text.is_empty() {
                primary.cedar_text = rule.clone();
            } else {
                primary.cedar_text.push('\n');
                primary.cedar_text.push_str(&rule);
            }
            primary.created_at = created_at.clone();
            primary.created_by = "api".to_string();
            primary.enabled = true;
        } else {
            rows.push(PolicyStoreRow {
                tenant: tenant.clone(),
                policy_id: "primary".to_string(),
                cedar_text: rule.clone(),
                policy_hash: String::new(),
                created_at: created_at.clone(),
                created_by: "api".to_string(),
                enabled: true,
            });
        }
        Ok(())
    })
    .await
    {
        return mutation_error_response(error);
    }

    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Policies);

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"tenant": tenant, "status": "rule_added"})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// New individual policy management endpoints (Phase 1)
// ---------------------------------------------------------------------------

/// GET /api/tenants/{tenant}/policies/list — list individual policy entries.
///
/// Returns structured JSON with per-policy details (id, cedar_text, enabled, etc.).
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/policies/list"))]
pub(crate) async fn handle_list_policies(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    _auth: PolicyAuthed,
) -> impl IntoResponse {
    let Some(store) = state.policy_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Persistence backend not configured",
        )
            .into_response();
    };

    match store.load_policies_for_tenant(&tenant).await {
        Ok(rows) => {
            let enabled_count = rows.iter().filter(|r| r.enabled).count();
            let disabled_count = rows.len() - enabled_count;
            let policies: Vec<serde_json::Value> = rows.iter().map(policy_row_to_json).collect();
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "tenant": tenant,
                    "policies": policies,
                    "total": rows.len(),
                    "enabled_count": enabled_count,
                    "disabled_count": disabled_count,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, tenant, "failed to list policies");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to list policies: {e}"),
            )
                .into_response()
        }
    }
}

/// GET /api/policies — list policies across all tenants (admin only).
#[instrument(skip_all, fields(otel.name = "GET /api/policies"))]
pub(crate) async fn handle_list_all_policies(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(status) =
        crate::authz::require_observe_auth(&state, &headers, "manage_policies", "PolicySet")
    {
        return (status, "Authorization required for cross-tenant access").into_response();
    }

    let Some(store) = state.policy_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Persistence backend not configured",
        )
            .into_response();
    };

    let mut rows = match store.load_all_policies().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, "failed to list policies from durable store");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to list policies: {e}"),
            )
                .into_response();
        }
    };

    rows.sort_by(|a, b| {
        a.tenant
            .cmp(&b.tenant)
            .then_with(|| a.policy_id.cmp(&b.policy_id))
            .then_with(|| a.created_at.cmp(&b.created_at))
    });

    let total = rows.len();
    let mut by_tenant = std::collections::BTreeMap::new();
    for row in &rows {
        *by_tenant.entry(row.tenant.clone()).or_insert(0usize) += 1;
    }
    let policies: Vec<serde_json::Value> = rows.iter().map(policy_row_to_json).collect();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "policies": policies,
            "total": total,
            "by_tenant": by_tenant,
        })),
    )
        .into_response()
}

/// POST /api/tenants/{tenant}/policies/create — create a new individual policy.
///
/// Request body: `{ "policy_id": "my-policy", "cedar_text": "permit(...);" }`
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, otel.name = "POST /api/tenants/{tenant}/policies/create"))]
pub(crate) async fn handle_create_policy(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    _auth: PolicyAuthed,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let policy_id = match body.get("policy_id").and_then(|v| v.as_str()) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "Missing or empty 'policy_id' field",
            )
                .into_response();
        }
    };
    let cedar_text = match body.get("cedar_text").and_then(|v| v.as_str()) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "Missing or empty 'cedar_text' field",
            )
                .into_response();
        }
    };
    if is_decision_policy(&policy_id) {
        return (
            StatusCode::CONFLICT,
            "The decision: policy namespace is reserved for approved decisions",
        )
            .into_response();
    }

    let created_by = body
        .get("created_by")
        .and_then(|v| v.as_str())
        .unwrap_or("api")
        .to_string();
    let created_at = sim_now().to_rfc3339();
    if let Err(error) = mutate_tenant_policies(&state, &tenant, |rows| {
        if rows.iter().any(|row| row.policy_id == policy_id) {
            return Err(PolicyMutationError::AlreadyExists);
        }
        rows.push(PolicyStoreRow {
            tenant: tenant.clone(),
            policy_id: policy_id.clone(),
            cedar_text: cedar_text.clone(),
            policy_hash: String::new(),
            created_at: created_at.clone(),
            created_by: created_by.clone(),
            enabled: true,
        });
        Ok(())
    })
    .await
    {
        return mutation_error_response(error);
    }

    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Policies);

    (
        StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "tenant": tenant,
            "policy_id": policy_id,
            "status": "created",
        })),
    )
        .into_response()
}

/// PATCH /api/tenants/{tenant}/policies/entry/{policy_id} — update an individual policy.
///
/// Request body (all fields optional):
/// `{ "cedar_text": "...", "enabled": true }`
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, policy_id, otel.name = "PATCH /api/tenants/{tenant}/policies/entry/{policy_id}"))]
pub(crate) async fn handle_patch_policy(
    State(state): State<ServerState>,
    Path((tenant, policy_id)): Path<(String, String)>,
    _auth: PolicyAuthed,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let new_cedar_text = body
        .get("cedar_text")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let new_enabled = body.get("enabled").and_then(|v| v.as_bool());

    if is_decision_policy(&policy_id) && new_cedar_text.is_some() {
        return (
            StatusCode::CONFLICT,
            "Approved decision policy content is immutable; disable or delete it to revoke",
        )
            .into_response();
    }

    if new_cedar_text.is_none() && new_enabled.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "Request body must contain 'cedar_text' and/or 'enabled'",
        )
            .into_response();
    }

    let created_by = body
        .get("created_by")
        .and_then(|value| value.as_str())
        .unwrap_or("api")
        .to_string();
    let created_at = sim_now().to_rfc3339();
    if let Err(error) = mutate_tenant_policies(&state, &tenant, |rows| {
        let row = rows
            .iter_mut()
            .find(|row| row.policy_id == policy_id)
            .ok_or(PolicyMutationError::NotFound)?;
        if let Some(cedar_text) = &new_cedar_text {
            row.cedar_text.clone_from(cedar_text);
            row.created_by.clone_from(&created_by);
            row.created_at.clone_from(&created_at);
        }
        if let Some(enabled) = new_enabled {
            row.enabled = enabled;
        }
        Ok(())
    })
    .await
    {
        return mutation_error_response(error);
    }

    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Policies);

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "tenant": tenant,
            "policy_id": policy_id,
            "status": "updated",
        })),
    )
        .into_response()
}

/// DELETE /api/tenants/{tenant}/policies/entry/{policy_id} — delete an individual policy.
///
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, policy_id, otel.name = "DELETE /api/tenants/{tenant}/policies/entry/{policy_id}"))]
pub(crate) async fn handle_delete_policy_entry(
    State(state): State<ServerState>,
    Path((tenant, policy_id)): Path<(String, String)>,
    _auth: PolicyAuthed,
) -> impl IntoResponse {
    if let Err(error) = mutate_tenant_policies(&state, &tenant, |rows| {
        let prior_len = rows.len();
        rows.retain(|row| row.policy_id != policy_id);
        if rows.len() == prior_len {
            return Err(PolicyMutationError::NotFound);
        }
        Ok(())
    })
    .await
    {
        return mutation_error_response(error);
    }

    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Policies);

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "tenant": tenant,
            "policy_id": policy_id,
            "status": "deleted",
        })),
    )
        .into_response()
}
