//! Policy management API endpoints.
//!
//! Handles Cedar policy CRUD operations for tenants, including full replacement,
//! incremental rule addition, individual policy listing/toggling/editing/deletion,
//! and credential-tenant policy views.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use tracing::instrument;

use super::{PolicyAuthed, PolicyReadAuthed};
use crate::state::ServerState;

mod replacement;
pub(super) mod support;
pub(crate) use replacement::handle_replace_policy;
use support::{
    build_prospective_enabled_text_with_override, live_text_with_entry, persist_and_reload_entry,
    persist_new_rule, policy_row_to_json, reload_tenant_from_store,
};

// ---------------------------------------------------------------------------
// Existing endpoints (unchanged interface, kept for backward compatibility)
// ---------------------------------------------------------------------------

/// GET /api/tenants/{tenant}/policies — return current Cedar policy text.
///
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/policies"))]
pub(crate) async fn handle_get_policies(
    State(state): State<ServerState>,
    Path(_tenant): Path<String>,
    auth: PolicyAuthed,
) -> impl IntoResponse {
    let tenant = auth.tenant().as_str().to_string();
    let policies = state.tenant_policies.read().unwrap(); // ci-ok: infallible lock
    let text = policies.get(&tenant).cloned().unwrap_or_default();
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"tenant": tenant, "policy_text": text})),
    )
        .into_response()
}

/// PUT /api/tenants/{tenant}/policies — replace all policies.
///
/// The text is validated in a scratch engine, stored as the tenant's only
/// enabled entry (other entries stay listed, disabled), and only then made
/// live, so a reload or restart activates exactly this text.
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, otel.name = "PUT /api/tenants/{tenant}/policies"))]
pub(crate) async fn handle_put_policies(
    State(state): State<ServerState>,
    Path(_tenant): Path<String>,
    auth: PolicyAuthed,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let _policy_guard = state.policy_approval_lock.lock().await;
    let tenant = auth.tenant().as_str().to_string();
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

    if let Err(e) = temper_authz::AuthzEngine::empty().reload_tenant_policies(&tenant, &policy_text)
    {
        tracing::warn!(error = %e, "policy validation failed");
        return (
            StatusCode::BAD_REQUEST,
            format!("Policy validation failed: {e}"),
        )
            .into_response();
    }

    let created_by = auth.security_context().principal.id.as_str();
    if let Some(store) = state.policy_store()
        && let Err(e) = store
            .replace_tenant_policies(&tenant, "primary", &policy_text, created_by)
            .await
    {
        tracing::warn!(error = %e, tenant, "failed to store replacement policies");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Policy write failed; nothing changed",
        )
            .into_response();
    }

    if let Err(resp) = super::validate_and_reload_policies(&state, &tenant, &policy_text) {
        return resp;
    }
    {
        let mut policies = state.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        policies.insert(tenant.clone(), policy_text);
    }
    crate::authz::record_policy_change(&state, &tenant, "primary", created_by);

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
    Path(_tenant): Path<String>,
    auth: PolicyAuthed,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let _policy_guard = state.policy_approval_lock.lock().await;
    let tenant = auth.tenant().as_str().to_string();
    let body_json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "invalid JSON in add policy rule request");
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")).into_response();
        }
    };

    let rule = match body_json.get("rule").and_then(|v| v.as_str()) {
        Some(v) if !v.trim().is_empty() => v.to_string(),
        _ => {
            tracing::warn!("missing 'rule' field in add policy rule request");
            return (
                StatusCode::BAD_REQUEST,
                "Missing 'rule' field in request body",
            )
                .into_response();
        }
    };

    persist_new_rule(
        &state,
        &tenant,
        &rule,
        &auth.security_context().principal.id,
    )
    .await
}

// ---------------------------------------------------------------------------
// New individual policy management endpoints (Phase 1)
// ---------------------------------------------------------------------------

/// GET /api/tenants/{tenant}/policies/list — list individual policy entries.
///
/// Returns structured JSON with per-policy details (id, cedar_text, enabled, etc.).
/// Cedar-gated: requires `read_policies` or `manage_policies` on `PolicySet`.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/policies/list"))]
pub(crate) async fn handle_list_policies(
    State(state): State<ServerState>,
    Path(_tenant): Path<String>,
    auth: PolicyReadAuthed,
) -> impl IntoResponse {
    let tenant = auth.tenant().as_str().to_string();
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

/// GET /api/policies — list policies in the credential-bound tenant.
#[instrument(skip_all, fields(otel.name = "GET /api/policies"))]
pub(crate) async fn handle_list_all_policies(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let authenticated = match crate::authz::require_authenticated_context(authenticated.as_deref())
    {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    if let Err(status) =
        crate::authz::require_observe_auth(&state, authenticated, "manage_policies", "PolicySet")
    {
        return (status, "Authorization required").into_response();
    }

    let Some(store) = state.policy_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Persistence backend not configured",
        )
            .into_response();
    };

    let tenant = authenticated.tenant().as_str();
    let mut rows = match store.load_policies_for_tenant(tenant).await {
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
    Path(_tenant): Path<String>,
    auth: PolicyAuthed,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let _policy_guard = state.policy_approval_lock.lock().await;
    let tenant = auth.tenant().as_str().to_string();
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

    // The live text with this entry set: its old statements leave, so a
    // repeated or replacing create never stacks copies.
    // A create that changes nothing (the row and live set already hold this
    // text) reloads and writes nothing.
    match live_text_with_entry(&state, &tenant, &policy_id, &cedar_text).await {
        Err(refused) => return refused.into_response(),
        Ok(None) => {}
        Ok(Some(prospective)) => {
            let created_by = auth.security_context().principal.id.as_str();
            if let Err(resp) = persist_and_reload_entry(
                &state,
                &tenant,
                &policy_id,
                &cedar_text,
                created_by,
                prospective,
            )
            .await
            {
                return resp;
            }
        }
    }

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
    Path((_tenant, policy_id)): Path<(String, String)>,
    auth: PolicyAuthed,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let _policy_guard = state.policy_approval_lock.lock().await;
    let tenant = auth.tenant().as_str().to_string();
    let Some(store) = state.policy_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Persistence backend not configured",
        )
            .into_response();
    };

    let new_cedar_text = body.get("cedar_text").and_then(|v| v.as_str());
    let new_enabled = body.get("enabled").and_then(|v| v.as_bool());

    if new_cedar_text.is_none() && new_enabled.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "Request body must contain 'cedar_text' and/or 'enabled'",
        )
            .into_response();
    }

    // If cedar_text is being changed, validate it first.
    if let Some(cedar_text) = new_cedar_text {
        // Validate by building prospective text for the tenant.
        let prospective = build_prospective_enabled_text_with_override(
            &state,
            &tenant,
            &policy_id,
            cedar_text,
            new_enabled,
        )
        .await;
        if let Err(resp) = super::validate_and_reload_policies(&state, &tenant, &prospective) {
            return resp;
        }

        let created_by = auth.security_context().principal.id.as_str();
        if let Err(e) = store
            .update_policy_text(&tenant, &policy_id, cedar_text, created_by)
            .await
        {
            tracing::warn!(error = %e, "failed to update policy text");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to update policy: {e}"),
            )
                .into_response();
        }
    }

    // If enabled is being changed, toggle it.
    if let Some(enabled) = new_enabled {
        match store
            .toggle_policy_enabled(&tenant, &policy_id, enabled)
            .await
        {
            Ok(false) => {
                return (StatusCode::NOT_FOUND, "Policy not found").into_response();
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to toggle policy enabled");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to toggle policy: {e}"),
                )
                    .into_response();
            }
            Ok(true) => {}
        }
    }

    // Reload tenant policies from durable storage to update in-memory state.
    reload_tenant_from_store(&state, &tenant).await;

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
    Path((_tenant, policy_id)): Path<(String, String)>,
    auth: PolicyAuthed,
) -> impl IntoResponse {
    let _policy_guard = state.policy_approval_lock.lock().await;
    let tenant = auth.tenant().as_str().to_string();
    let Some(store) = state.policy_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Persistence backend not configured",
        )
            .into_response();
    };

    if let Err(e) = store.delete_policy(&tenant, &policy_id).await {
        tracing::warn!(error = %e, "failed to delete policy");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to delete policy: {e}"),
        )
            .into_response();
    }

    // Reload tenant policies from durable storage to update in-memory state.
    reload_tenant_from_store(&state, &tenant).await;

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
