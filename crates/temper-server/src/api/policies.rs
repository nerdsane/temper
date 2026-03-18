//! Policy management API endpoints.
//!
//! Handles Cedar policy CRUD operations for tenants, including full replacement,
//! incremental rule addition, individual policy listing/toggling/editing/deletion,
//! and cross-tenant policy views.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use tracing::instrument;

use crate::authz::{load_and_activate_tenant_policies, persist_and_activate_policy};
use crate::state::ServerState;

/// Derive a human-readable source label from a `policy_id`.
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

/// Serialize a [`PolicyRow`] to a JSON value for API responses.
fn policy_row_to_json(row: &temper_store_turso::PolicyRow) -> serde_json::Value {
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

// ---------------------------------------------------------------------------
// Existing endpoints (unchanged interface, kept for backward compatibility)
// ---------------------------------------------------------------------------

/// GET /api/tenants/{tenant}/policies — return current Cedar policy text.
///
/// Cedar-gated: requires `manage_policies` action on `PolicySet` resource.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/policies"))]
pub(crate) async fn handle_get_policies(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(resp) = super::require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }
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
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if let Some(resp) = super::require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }

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

    if let Err(resp) = super::validate_and_reload_policies(&state, &tenant, &policy_text) {
        return resp;
    }

    {
        let mut policies = state.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        policies.insert(tenant.clone(), policy_text.clone());
    }

    persist_and_activate_policy(&state, &tenant, "primary", &policy_text, "api").await;

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
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if let Some(resp) = super::require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }

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

    let new_tenant_text = {
        let policies = state.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        let existing = policies.get(&tenant).cloned().unwrap_or_default();
        if existing.is_empty() {
            rule.clone()
        } else {
            format!("{existing}\n{rule}")
        }
    };

    if let Err(resp) = super::validate_and_reload_policies(&state, &tenant, &new_tenant_text) {
        return resp;
    }

    {
        let mut policies = state.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        policies.insert(tenant.clone(), new_tenant_text.clone());
    }

    persist_and_activate_policy(&state, &tenant, "primary", &new_tenant_text, "api").await;

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
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(resp) = super::require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }

    let Some(turso) = state.persistent_store_for_tenant(&tenant).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Persistence backend not configured",
        )
            .into_response();
    };

    match turso.load_policies_for_tenant(&tenant).await {
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

    let stores = state.collect_all_turso_stores().await;
    if stores.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Persistence backend not configured",
        )
            .into_response();
    };

    let mut rows: Vec<temper_store_turso::PolicyRow> = Vec::new();
    for turso in &stores {
        match turso.load_all_policies().await {
            Ok(mut loaded) => rows.append(&mut loaded),
            Err(e) => tracing::warn!(error = %e, "failed to list policies from Turso store"),
        }
    }

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
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Some(resp) = super::require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }

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

    // Validate: build prospective enabled policy text with the new entry added.
    let prospective =
        build_prospective_enabled_text(&state, &tenant, Some((&policy_id, &cedar_text))).await;
    if let Err(resp) = super::validate_and_reload_policies(&state, &tenant, &prospective) {
        return resp;
    }

    // Persist the new policy entry.
    let created_by = body
        .get("created_by")
        .and_then(|v| v.as_str())
        .unwrap_or("api");
    persist_and_activate_policy(&state, &tenant, &policy_id, &cedar_text, created_by).await;

    // Update in-memory map to match the prospective text.
    {
        let mut policies = state.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        policies.insert(tenant.clone(), prospective);
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
    Path((tenant, policy_id)): Path<(String, String)>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Some(resp) = super::require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }

    let Some(turso) = state.persistent_store_for_tenant(&tenant).await else {
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

        let created_by = body
            .get("created_by")
            .and_then(|v| v.as_str())
            .unwrap_or("api");
        if let Err(e) = turso
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
        match turso
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

    // Reload tenant policies from Turso to update in-memory state.
    reload_tenant_from_turso(&state, &tenant).await;

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
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(resp) = super::require_policy_auth(&state, &headers, &tenant).await {
        return resp;
    }

    let Some(turso) = state.persistent_store_for_tenant(&tenant).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Persistence backend not configured",
        )
            .into_response();
    };

    if let Err(e) = turso.delete_policy(&tenant, &policy_id).await {
        tracing::warn!(error = %e, "failed to delete policy");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to delete policy: {e}"),
        )
            .into_response();
    }

    // Reload tenant policies from Turso to update in-memory state.
    reload_tenant_from_turso(&state, &tenant).await;

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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Reload a tenant's in-memory policy state from Turso.
///
/// Reads all enabled policies, concatenates them, updates `tenant_policies`,
/// and reloads the Cedar engine.
async fn reload_tenant_from_turso(state: &ServerState, tenant: &str) {
    let Some(turso) = state.persistent_store_for_tenant(tenant).await else {
        return;
    };
    load_and_activate_tenant_policies(state, tenant, &turso).await;
}

/// Build the prospective enabled policy text for a tenant, optionally including
/// a new policy entry that isn't persisted yet.
async fn build_prospective_enabled_text(
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

/// Build the prospective enabled policy text for a tenant, replacing one
/// specific policy entry's text and/or enabled state.
async fn build_prospective_enabled_text_with_override(
    state: &ServerState,
    tenant: &str,
    override_policy_id: &str,
    override_cedar_text: &str,
    override_enabled: Option<bool>,
) -> String {
    // Load all current policies from Turso to get accurate per-entry data.
    let rows = if let Some(turso) = state.persistent_store_for_tenant(tenant).await {
        turso
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
