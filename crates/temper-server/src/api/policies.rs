//! Policy management API endpoints.
//!
//! Handles Cedar policy CRUD operations for tenants, including full replacement
//! and incremental rule addition with validation.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use tracing::instrument;

use crate::authz::persist_and_activate_policy;
use crate::state::ServerState;

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
    // Cedar authorization gate — shared with decisions.
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

    // Validate by combining all tenants' policies and reloading.
    if let Err(resp) = super::validate_and_reload_policies(&state, &tenant, &policy_text) {
        return resp;
    }

    // Store the tenant policy in-memory.
    {
        let mut policies = state.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        policies.insert(tenant.clone(), policy_text.clone());
    }

    // Persist to Turso `policies` table (hash-gated; logs trajectory on change).
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
    // Cedar authorization gate — shared with decisions.
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

    // Build new tenant text = existing + rule, then validate combined policies.
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

    // Persist updated tenant policy in-memory.
    {
        let mut policies = state.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        policies.insert(tenant.clone(), new_tenant_text.clone());
    }

    // Persist to Turso `policies` table (hash-gated; logs trajectory on change).
    persist_and_activate_policy(&state, &tenant, "primary", &new_tenant_text, "api").await;

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"tenant": tenant, "status": "rule_added"})),
    )
        .into_response()
}
