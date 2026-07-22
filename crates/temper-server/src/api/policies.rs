//! Policy management API endpoints.
//!
//! Handles Cedar policy CRUD operations for tenants, including full replacement,
//! incremental rule addition, individual policy listing/toggling/editing/deletion,
//! and cross-tenant policy views.

mod listing;
mod publication;

pub(crate) use listing::{handle_list_all_policies, handle_list_policies};
use publication::{
    PolicyUpsert, activate_policy_generation, arm_policy_generation,
    begin_durable_policy_generation, begin_policy_generation_mutation,
    begin_policy_generation_read, policy_generation_intent, policy_generation_writes,
    publish_memory_policy_generation, publish_policy_upsert_mode, validate_policy_generation,
};
pub(super) use publication::{publish_policy_replace_all, publish_policy_upsert};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tracing::instrument;

use super::PolicyAuthed;
use crate::authz::policy_persistence::persist_complete_policy_generation;
use crate::state::ServerState;

#[cfg(test)]
mod tests;

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
    mut auth: PolicyAuthed,
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

    let expected_generation = auth.release_for_publication();
    let auth_headers = auth.headers().clone();
    if state.policy_store().is_some() {
        if let Err(response) = publish_policy_replace_all(
            &state,
            &tenant,
            &policy_text,
            "api",
            Some(expected_generation),
            Some(&auth_headers),
        )
        .await
        {
            return response;
        }
    } else {
        let mut generation_writer = match begin_policy_generation_mutation(
            &state,
            &tenant,
            Some(expected_generation),
            Some(&auth_headers),
        )
        .await
        {
            Ok(guard) => guard,
            Err(response) => return response,
        };
        if let Err(response) = publish_memory_policy_generation(
            &state,
            &tenant,
            &policy_text,
            "memory-policy-replace-v1",
            &mut generation_writer,
        ) {
            return response;
        }
    }

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
    mut auth: PolicyAuthed,
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

    let expected_generation = auth.release_for_publication();
    let auth_headers = auth.headers().clone();
    if state.policy_store().is_some() {
        if let Err(response) = publish_policy_upsert_mode(
            &state,
            &tenant,
            "primary",
            PolicyUpsert::AppendRule(rule),
            "api",
            Some(expected_generation),
            Some(&auth_headers),
        )
        .await
        {
            return response;
        }
    } else {
        let mut generation_writer = match begin_policy_generation_mutation(
            &state,
            &tenant,
            Some(expected_generation),
            Some(&auth_headers),
        )
        .await
        {
            Ok(guard) => guard,
            Err(response) => return response,
        };
        let new_tenant_text = {
            let policies = state
                .tenant_policies
                .read()
                .expect("tenant policy lock poisoned");
            let existing = policies.get(&tenant).cloned().unwrap_or_default();
            if existing.is_empty() {
                rule
            } else {
                format!("{existing}\n{rule}")
            }
        };
        if let Err(response) = publish_memory_policy_generation(
            &state,
            &tenant,
            &new_tenant_text,
            "memory-policy-append-v1",
            &mut generation_writer,
        ) {
            return response;
        }
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"tenant": tenant, "status": "rule_added"})),
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
    mut auth: PolicyAuthed,
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

    let created_by = body
        .get("created_by")
        .and_then(|v| v.as_str())
        .unwrap_or("api");
    let expected_generation = auth.release_for_publication();
    let auth_headers = auth.headers().clone();
    if state.policy_store().is_some() {
        if let Err(response) = publish_policy_upsert(
            &state,
            &tenant,
            &policy_id,
            &cedar_text,
            created_by,
            Some(expected_generation),
            Some(&auth_headers),
        )
        .await
        {
            return response;
        }
    } else {
        let mut generation_writer = match begin_policy_generation_mutation(
            &state,
            &tenant,
            Some(expected_generation),
            Some(&auth_headers),
        )
        .await
        {
            Ok(guard) => guard,
            Err(response) => return response,
        };
        let prospective = {
            let policies = state
                .tenant_policies
                .read()
                .expect("tenant policy lock poisoned");
            let existing = policies.get(&tenant).cloned().unwrap_or_default();
            if existing.is_empty() {
                cedar_text.clone()
            } else {
                format!("{existing}\n{cedar_text}")
            }
        };
        if let Err(response) = publish_memory_policy_generation(
            &state,
            &tenant,
            &prospective,
            "memory-policy-create-v1",
            &mut generation_writer,
        ) {
            return response;
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
    Path((tenant, policy_id)): Path<(String, String)>,
    mut auth: PolicyAuthed,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> impl IntoResponse {
    let new_cedar_text = body.get("cedar_text").and_then(|v| v.as_str());
    let new_enabled = body.get("enabled").and_then(|v| v.as_bool());

    if new_cedar_text.is_none() && new_enabled.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            "Request body must contain 'cedar_text' and/or 'enabled'",
        )
            .into_response();
    }

    let expected_generation = auth.release_for_publication();
    let auth_headers = auth.headers().clone();
    let (mut generation_writer, _store, mut entries) = match begin_durable_policy_generation(
        &state,
        &tenant,
        Some(expected_generation),
        Some(&auth_headers),
    )
    .await
    {
        Ok(generation) => generation,
        Err(response) => return response,
    };
    let Some(entry_index) = entries
        .iter()
        .position(|entry| entry.policy_id == policy_id)
    else {
        return (StatusCode::NOT_FOUND, "Policy not found").into_response();
    };
    if let Some(cedar_text) = new_cedar_text {
        entries[entry_index].cedar_text = cedar_text.to_string();
    }
    if let Some(enabled) = new_enabled {
        entries[entry_index].enabled = enabled;
    }
    if let Err(response) = validate_policy_generation(&tenant, &entries) {
        return response;
    }
    let created_by = body
        .get("created_by")
        .and_then(|v| v.as_str())
        .unwrap_or("api");
    entries[entry_index].created_by = created_by.to_string();
    let enabled_component = new_enabled
        .map(|enabled| enabled.to_string())
        .unwrap_or_default();
    let intent = policy_generation_intent(
        "direct-policy-patch-v1",
        &entries,
        &[
            ("policy-id", policy_id.as_bytes()),
            ("cedar-text", new_cedar_text.unwrap_or_default().as_bytes()),
            ("enabled", enabled_component.as_bytes()),
            ("created-by", created_by.as_bytes()),
        ],
    );
    if let Err(response) = arm_policy_generation(&state, &mut generation_writer, &tenant, &intent) {
        return response;
    }
    if let Err(error) = persist_complete_policy_generation(
        &state,
        &tenant,
        &policy_generation_writes(&entries),
        &policy_id,
        created_by,
    )
    .await
    {
        tracing::warn!(%error, tenant, policy_id, "failed to replace policy generation");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to update policy: {error}"),
        )
            .into_response();
    }
    if let Err(response) =
        activate_policy_generation(&state, &tenant, &entries, &mut generation_writer)
    {
        return response;
    }

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
    mut auth: PolicyAuthed,
) -> impl IntoResponse {
    let expected_generation = auth.release_for_publication();
    let auth_headers = auth.headers().clone();
    let (mut generation_writer, _store, mut entries) = match begin_durable_policy_generation(
        &state,
        &tenant,
        Some(expected_generation),
        Some(&auth_headers),
    )
    .await
    {
        Ok(generation) => generation,
        Err(response) => return response,
    };
    entries.retain(|entry| entry.policy_id != policy_id);
    if let Err(response) = validate_policy_generation(&tenant, &entries) {
        return response;
    }
    let intent = policy_generation_intent(
        "direct-policy-delete-v1",
        &entries,
        &[("policy-id", policy_id.as_bytes())],
    );
    if let Err(response) = arm_policy_generation(&state, &mut generation_writer, &tenant, &intent) {
        return response;
    }
    if let Err(error) = persist_complete_policy_generation(
        &state,
        &tenant,
        &policy_generation_writes(&entries),
        &policy_id,
        "api",
    )
    .await
    {
        tracing::warn!(%error, tenant, policy_id, "failed to delete policy generation");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to delete policy: {error}"),
        )
            .into_response();
    }
    if let Err(response) =
        activate_policy_generation(&state, &tenant, &entries, &mut generation_writer)
    {
        return response;
    }

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
