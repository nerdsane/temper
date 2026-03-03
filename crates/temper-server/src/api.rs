//! Management API routes (mutations).
//!
//! These endpoints handle spec loading, WASM module management, and evolution
//! decisions.  They are separated from the read-only `/observe` router so that
//! observe stays purely observational.

use std::convert::Infallible;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post, put};
use temper_authz::PrincipalKind;
use temper_runtime::scheduler::sim_now;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use tracing::instrument;

use crate::authz_helpers::{record_authz_denial, security_context_from_headers};
use crate::state::{
    DecisionStatus, PendingDecision, PolicyScope, ServerState, TrajectoryEntry, TrajectorySource,
};
use temper_evolution::records::{Decision, DecisionRecord, RecordHeader, RecordType};

/// Build the management API router (mounted at /api).
///
/// Route structure:
/// - POST   /api/specs/load-dir                        → load specs from directory
/// - POST   /api/specs/load-inline                     → load specs from inline payload
/// - POST   /api/wasm/modules/{module_name}            → upload WASM module
/// - DELETE /api/wasm/modules/{module_name}             → delete WASM module
/// - POST   /api/evolution/records/{id}/decide          → developer decision on record
/// - POST   /api/evolution/trajectories/unmet           → report unmet user intent
/// - POST   /api/evolution/sentinel/check               → trigger sentinel health check
pub fn build_api_router() -> Router<ServerState> {
    Router::new()
        .route(
            "/specs/load-dir",
            post(crate::observe::specs::handle_load_dir),
        )
        .route(
            "/specs/load-inline",
            post(crate::observe::specs::handle_load_inline),
        )
        .route(
            "/wasm/modules/{module_name}",
            post(crate::observe::wasm::upload_wasm_module)
                .delete(crate::observe::wasm::delete_wasm_module),
        )
        .route(
            "/evolution/records/{id}/decide",
            post(crate::observe::evolution::handle_decide),
        )
        .route(
            "/evolution/trajectories/unmet",
            post(crate::observe::evolution::handle_unmet_intent),
        )
        .route(
            "/evolution/sentinel/check",
            post(crate::observe::evolution::handle_sentinel_check),
        )
        .route(
            "/tenants/{tenant}/secrets/{key_name}",
            put(handle_put_secret).delete(handle_delete_secret),
        )
        .route("/tenants/{tenant}/secrets", get(handle_list_secrets))
        // Policy CRUD (Phase 3)
        .route(
            "/tenants/{tenant}/policies",
            get(handle_get_policies).put(handle_put_policies),
        )
        .route(
            "/tenants/{tenant}/policies/rules",
            post(handle_add_policy_rule),
        )
        // Decision approve/deny (Phase 4)
        .route("/tenants/{tenant}/decisions", get(handle_list_decisions))
        .route(
            "/tenants/{tenant}/decisions/stream",
            get(handle_decision_stream),
        )
        .route(
            "/tenants/{tenant}/decisions/{id}/approve",
            post(handle_approve_decision),
        )
        .route(
            "/tenants/{tenant}/decisions/{id}/deny",
            post(handle_deny_decision),
        )
        // REPL endpoint (Monty sandbox over HTTP)
        .route("/repl", post(handle_repl))
        // Agent authorization + audit endpoints
        .route("/authorize", post(handle_authorize))
        .route("/audit", post(handle_audit))
        // Cross-tenant decision endpoints
        .route("/decisions", get(handle_list_all_decisions))
        .route("/decisions/stream", get(handle_all_decisions_stream))
        // Agent progress SSE endpoint
        .route(
            "/agents/{agent_id}/stream",
            get(handle_agent_progress_stream),
        )
}

async fn authorize_tenant_decision_management(
    state: &ServerState,
    headers: &HeaderMap,
    tenant: &str,
) -> Option<axum::response::Response> {
    let security_ctx = security_context_from_headers(headers, None, None);
    if matches!(security_ctx.principal.kind, PrincipalKind::Admin) {
        // Admin principals (e.g. Observe UI) always bypass Cedar for decision
        // management. Without this, approving the first policy would lock out
        // the admin from managing subsequent decisions.
        return None;
    }
    if let Err(reason) = state.authorize_with_context(
        &security_ctx,
        "manage_policies",
        "PolicySet",
        &std::collections::BTreeMap::new(),
    ) {
        let pd = record_authz_denial(
            state,
            tenant,
            &security_ctx,
            None,
            "manage_policies",
            "PolicySet",
            tenant,
            serde_json::json!({"tenant": tenant}),
            &reason,
            None,
            None,
        )
        .await;
        return Some(
            (
                StatusCode::FORBIDDEN,
                axum::Json(serde_json::json!({
                    "error": {
                        "code": "AuthorizationDenied",
                        "message": format!("{reason} Decision {}", pd.id),
                    }
                })),
            )
                .into_response(),
        );
    }
    None
}

fn is_backend_not_supported_error(err: &str) -> bool {
    err.to_ascii_lowercase().contains("not supported")
}

/// PUT /api/tenants/{tenant}/secrets/{key_name} — encrypt and store a secret.
#[instrument(skip_all, fields(tenant, key_name, otel.name = "PUT /api/tenants/{tenant}/secrets/{key_name}"))]
async fn handle_put_secret(
    State(state): State<ServerState>,
    Path((tenant, key_name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!("secrets vault not configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Secrets vault not configured (missing TEMPER_VAULT_KEY)",
        )
            .into_response();
    };

    let body_json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "invalid JSON in put secret request");
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")).into_response();
        }
    };

    let value = match body_json.get("value").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => {
            tracing::warn!("missing 'value' field in put secret request");
            return (
                StatusCode::BAD_REQUEST,
                "Missing \'value\' field in request body",
            )
                .into_response();
        }
    };

    if value.len() > crate::secrets_vault::MAX_SECRET_VALUE_BYTES {
        tracing::warn!(size = value.len(), "secret value exceeds maximum size");
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "Secret value exceeds maximum size of {} bytes",
                crate::secrets_vault::MAX_SECRET_VALUE_BYTES
            ),
        )
            .into_response();
    }

    // Encrypt the value.
    let (ciphertext, nonce) = match vault.encrypt(value.as_bytes()) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(error = %e, "encryption failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encryption failed: {e}"),
            )
                .into_response();
        }
    };

    // Persist first.
    if let Err(e) = state
        .upsert_secret(&tenant, &key_name, &ciphertext, &nonce)
        .await
    {
        let status = if is_backend_not_supported_error(&e) {
            StatusCode::NOT_IMPLEMENTED
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        tracing::error!(error = %e, "secret persistence failed");
        return (status, format!("Persistence failed: {e}")).into_response();
    }

    // Cache in memory after successful persistence.
    if let Err(e) = vault.cache_secret(&tenant, &key_name, value.to_string()) {
        // Best-effort rollback to keep storage/cache aligned.
        let _ = state.delete_secret(&tenant, &key_name).await;
        tracing::error!(error = %e, "cache update failed after persistence write");
        return (
            StatusCode::CONFLICT,
            format!("Cache update failed after persistence write: {e}"),
        )
            .into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

/// DELETE /api/tenants/{tenant}/secrets/{key_name} — remove a secret.
#[instrument(skip_all, fields(tenant, key_name, otel.name = "DELETE /api/tenants/{tenant}/secrets/{key_name}"))]
async fn handle_delete_secret(
    State(state): State<ServerState>,
    Path((tenant, key_name)): Path<(String, String)>,
) -> impl IntoResponse {
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!("secrets vault not configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Secrets vault not configured",
        )
            .into_response();
    };

    match state.delete_secret(&tenant, &key_name).await {
        Ok(true) => {
            vault.remove_secret(&tenant, &key_name);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => {
            tracing::warn!("secret not found for deletion");
            vault.remove_secret(&tenant, &key_name);
            StatusCode::NOT_FOUND.into_response()
        }
        Err(e) => {
            let status = if is_backend_not_supported_error(&e) {
                StatusCode::NOT_IMPLEMENTED
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            tracing::error!(error = %e, "secret deletion failed");
            (status, format!("Delete failed: {e}")).into_response()
        }
    }
}

/// GET /api/tenants/{tenant}/secrets — list secret key names (never values).
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/secrets"))]
async fn handle_list_secrets(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
) -> impl IntoResponse {
    let Some(vault) = state.secrets_vault.as_ref() else {
        tracing::warn!("secrets vault not configured");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"error": "Secrets vault not configured"})),
        )
            .into_response();
    };

    let keys = vault.list_keys(&tenant);
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"keys": keys})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Phase 3: Policy CRUD handlers
// ---------------------------------------------------------------------------

/// GET /api/tenants/{tenant}/policies — return current Cedar policy text.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/policies"))]
async fn handle_get_policies(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
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
async fn handle_put_policies(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Cedar authorization gate.
    let security_ctx = security_context_from_headers(&headers, None, None);
    let authz_result = state.authorize_with_context(
        &security_ctx,
        "manage_policies",
        "PolicySet",
        &std::collections::BTreeMap::new(),
    );
    if let Err(reason) = authz_result {
        tracing::warn!(reason = %reason, "authorization denied for put policies");
        let pd = record_authz_denial(
            &state,
            &tenant,
            &security_ctx,
            None,
            "manage_policies",
            "PolicySet",
            &tenant,
            serde_json::json!({"tenant": tenant}),
            &reason,
            None,
            None,
        )
        .await;
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "AuthorizationDenied",
                    "message": format!("{reason} Decision {}", pd.id),
                }
            })),
        )
            .into_response();
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

    // Validate by attempting to reload (dry-run combined text).
    let combined = {
        let policies = state.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        let mut all = String::new();
        for (t, text) in policies.iter() {
            if *t != tenant {
                all.push_str(text);
                all.push('\n');
            }
        }
        all.push_str(&policy_text);
        all
    };

    if let Err(e) = state.authz.reload_policies(&combined) {
        tracing::warn!(error = %e, "policy validation failed");
        return (
            StatusCode::BAD_REQUEST,
            format!("Policy validation failed: {e}"),
        )
            .into_response();
    }

    // Store the tenant policy.
    {
        let mut policies = state.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        policies.insert(tenant.clone(), policy_text);
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
async fn handle_add_policy_rule(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Cedar authorization gate.
    let security_ctx = security_context_from_headers(&headers, None, None);
    let authz_result = state.authorize_with_context(
        &security_ctx,
        "manage_policies",
        "PolicySet",
        &std::collections::BTreeMap::new(),
    );
    if let Err(reason) = authz_result {
        tracing::warn!(reason = %reason, "authorization denied for add policy rule");
        let pd = record_authz_denial(
            &state,
            &tenant,
            &security_ctx,
            None,
            "manage_policies",
            "PolicySet",
            &tenant,
            serde_json::json!({"tenant": tenant}),
            &reason,
            None,
            None,
        )
        .await;
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "AuthorizationDenied",
                    "message": format!("{reason} Decision {}", pd.id),
                }
            })),
        )
            .into_response();
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

    // Append and validate.
    let combined = {
        let policies = state.tenant_policies.read().unwrap(); // ci-ok: infallible lock
        let mut all = String::new();
        for (_t, text) in policies.iter() {
            all.push_str(text);
            all.push('\n');
        }
        // Append new rule to this tenant's existing text.
        let existing = policies.get(&tenant).cloned().unwrap_or_default();
        // We'll build combined of all other tenants + this tenant's text + new rule.
        let mut combined = String::new();
        for (t, text) in policies.iter() {
            if *t != tenant {
                combined.push_str(text);
                combined.push('\n');
            }
        }
        combined.push_str(&existing);
        combined.push('\n');
        combined.push_str(&rule);
        combined
    };

    if let Err(e) = state.authz.reload_policies(&combined) {
        tracing::warn!(error = %e, "policy rule validation failed");
        return (
            StatusCode::BAD_REQUEST,
            format!("Rule validation failed: {e}"),
        )
            .into_response();
    }

    // Persist updated tenant policy.
    {
        let mut policies = state.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        let entry = policies.entry(tenant.clone()).or_default();
        if !entry.is_empty() {
            entry.push('\n');
        }
        entry.push_str(&rule);
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"tenant": tenant, "status": "rule_added"})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Phase 4: Decision approve/deny handlers
// ---------------------------------------------------------------------------

/// Query parameters for listing decisions.
#[derive(serde::Deserialize)]
struct DecisionListParams {
    /// Optional status filter: "pending", "approved", "denied", "expired".
    status: Option<String>,
}

/// GET /api/tenants/{tenant}/decisions — list decisions with optional status filter.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/decisions"))]
async fn handle_list_decisions(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    _headers: HeaderMap,
    Query(params): Query<DecisionListParams>,
) -> impl IntoResponse {
    // Query Turso directly (single source of truth).
    if let Some(turso) = state.turso_opt() {
        match turso
            .query_decisions(&tenant, params.status.as_deref())
            .await
        {
            Ok(data_strings) => {
                let entries: Vec<serde_json::Value> = data_strings
                    .iter()
                    .filter_map(|s| serde_json::from_str(s).ok())
                    .collect();
                let pending_count = entries
                    .iter()
                    .filter(|d| d.get("status").and_then(|v| v.as_str()) == Some("pending"))
                    .count();
                let approved_count = entries
                    .iter()
                    .filter(|d| d.get("status").and_then(|v| v.as_str()) == Some("approved"))
                    .count();
                let denied_count = entries
                    .iter()
                    .filter(|d| d.get("status").and_then(|v| v.as_str()) == Some("denied"))
                    .count();
                let total = entries.len();
                return (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "decisions": entries,
                        "total": total,
                        "pending_count": pending_count,
                        "approved_count": approved_count,
                        "denied_count": denied_count,
                    })),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to query decisions from Turso");
            }
        }
    }

    // Fallback: empty response.
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "decisions": [],
            "total": 0,
            "pending_count": 0,
            "approved_count": 0,
            "denied_count": 0,
        })),
    )
        .into_response()
}

/// Body for approve request.
#[derive(serde::Deserialize)]
struct ApproveBody {
    /// Scope: "narrow", "medium", or "broad".
    scope: String,
    /// Optional: who approved.
    decided_by: Option<String>,
}

/// POST /api/tenants/{tenant}/decisions/{id}/approve — approve with scope.
#[instrument(skip_all, fields(tenant, id, otel.name = "POST /api/tenants/{tenant}/decisions/{id}/approve"))]
async fn handle_approve_decision(
    State(state): State<ServerState>,
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<ApproveBody>,
) -> impl IntoResponse {
    if let Some(resp) = authorize_tenant_decision_management(&state, &headers, &tenant).await {
        return resp;
    }

    let scope: PolicyScope = match body.scope.as_str() {
        "narrow" => PolicyScope::Narrow,
        "medium" => PolicyScope::Medium,
        "broad" => PolicyScope::Broad,
        other => {
            tracing::warn!(scope = %other, "invalid scope in approve decision request");
            return (
                StatusCode::BAD_REQUEST,
                format!("Invalid scope: {other}. Must be narrow, medium, or broad."),
            )
                .into_response();
        }
    };

    // Read decision from Turso (single source of truth).
    let mut decision: PendingDecision = {
        let Some(turso) = state.turso_opt() else {
            tracing::error!("Turso backend not configured for approve decision");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Turso backend not configured",
            )
                .into_response();
        };
        match turso.get_pending_decision(&id).await {
            Ok(Some(data_str)) => match serde_json::from_str::<PendingDecision>(&data_str) {
                Ok(d) if d.tenant == tenant => d,
                _ => {
                    tracing::warn!("decision not found for approval");
                    return (StatusCode::NOT_FOUND, "Decision not found").into_response();
                }
            },
            Ok(None) => {
                tracing::warn!("decision not found for approval");
                return (StatusCode::NOT_FOUND, "Decision not found").into_response();
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to load decision from Turso");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to load decision: {e}"),
                )
                    .into_response();
            }
        }
    };

    if decision.status != DecisionStatus::Pending {
        tracing::warn!(status = ?decision.status, "decision already resolved");
        return (
            StatusCode::CONFLICT,
            format!("Decision already resolved as {:?}", decision.status),
        )
            .into_response();
    }

    let generated_policy = decision.generate_policy(&scope);
    let evolution_record_id = decision.evolution_record_id.clone();

    // Build prospective policy set, validate+reload, then commit map atomically.
    {
        let mut policies = state.tenant_policies.write().unwrap(); // ci-ok: infallible lock
        let mut next_policies = policies.clone();
        let entry = next_policies.entry(tenant.clone()).or_default();
        if !entry.is_empty() {
            entry.push('\n');
        }
        entry.push_str(&generated_policy);

        let mut combined = String::new();
        for text in next_policies.values() {
            combined.push_str(text);
            combined.push('\n');
        }

        if let Err(e) = state.authz.reload_policies(&combined) {
            tracing::error!(error = %e, "failed to reload policies during approval");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to reload policies: {e}"),
            )
                .into_response();
        }

        *policies = next_policies.clone();
    }

    // Persist updated policies to Turso synchronously.
    if let Some(turso) = state.turso_opt() {
        let policies_snapshot = {
            let p = state.tenant_policies.read().unwrap(); // ci-ok: infallible lock
            p.clone()
        };
        for (t, text) in &policies_snapshot {
            if let Err(e) = turso.upsert_tenant_policy(t, text).await {
                eprintln!("Warning: failed to persist Cedar policy for tenant {t}: {e}");
            }
        }
    }

    // Mark decision approved only after policy reload succeeds.
    decision.status = DecisionStatus::Approved;
    decision.approved_scope = Some(scope);
    decision.generated_policy = Some(generated_policy.clone());
    decision.decided_by = body.decided_by.clone();
    decision.decided_at = Some(sim_now().to_rfc3339());
    let approved_decision = decision.clone();

    // Persist updated decision to Turso synchronously.
    if let Err(e) = state.persist_pending_decision(&approved_decision).await {
        eprintln!("Warning: failed to persist approved decision {}: {e}", id);
    }

    // Create D-Record for the approval (evolution audit trail).
    // Link to the A-Record via derived_from for O-A-D chain tracing.
    let d_header = RecordHeader::new(RecordType::Decision, "human:approval");
    let d_header = match evolution_record_id {
        Some(ref eid) => d_header.derived_from(eid.clone()),
        None => d_header,
    };
    let d_record = DecisionRecord {
        header: d_header,
        decision: Decision::Approved,
        decided_by: body
            .decided_by
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        rationale: format!(
            "Approved with scope: {:?}. Policy: {}",
            body.scope, generated_policy
        ),
        verification_results: None,
        implementation: None,
    };
    // Persist D-Record to Turso.
    if let Some(turso) = state.turso_opt() {
        let data_json = serde_json::to_string(&d_record).unwrap_or_default();
        if let Err(e) = turso
            .insert_evolution_record(
                &d_record.header.id,
                "Decision",
                &format!("{:?}", d_record.header.status),
                &d_record.header.created_by,
                d_record.header.derived_from.as_deref(),
                &data_json,
            )
            .await
        {
            tracing::warn!(error = %e, "failed to persist D-Record to Turso");
        }
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "id": id,
            "status": "approved",
            "generated_policy": generated_policy,
        })),
    )
        .into_response()
}

/// POST /api/tenants/{tenant}/decisions/{id}/deny — mark as denied.
#[instrument(skip_all, fields(tenant, id, otel.name = "POST /api/tenants/{tenant}/decisions/{id}/deny"))]
async fn handle_deny_decision(
    State(state): State<ServerState>,
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<axum::Json<serde_json::Value>>,
) -> impl IntoResponse {
    if let Some(resp) = authorize_tenant_decision_management(&state, &headers, &tenant).await {
        return resp;
    }

    let decided_by = body
        .as_ref()
        .and_then(|b| b.get("decided_by"))
        .and_then(|v| v.as_str())
        .map(String::from);

    // Read decision from Turso (single source of truth).
    let mut decision: PendingDecision = {
        let Some(turso) = state.turso_opt() else {
            tracing::error!("Turso backend not configured for deny decision");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Turso backend not configured",
            )
                .into_response();
        };
        match turso.get_pending_decision(&id).await {
            Ok(Some(data_str)) => match serde_json::from_str::<PendingDecision>(&data_str) {
                Ok(d) if d.tenant == tenant => d,
                _ => {
                    tracing::warn!("decision not found for denial");
                    return (StatusCode::NOT_FOUND, "Decision not found").into_response();
                }
            },
            Ok(None) => {
                tracing::warn!("decision not found for denial");
                return (StatusCode::NOT_FOUND, "Decision not found").into_response();
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to load decision from Turso");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to load decision: {e}"),
                )
                    .into_response();
            }
        }
    };

    if decision.status != DecisionStatus::Pending {
        tracing::warn!(status = ?decision.status, "decision already resolved");
        return (
            StatusCode::CONFLICT,
            format!("Decision already resolved as {:?}", decision.status),
        )
            .into_response();
    }

    decision.status = DecisionStatus::Denied;
    decision.decided_by = decided_by;
    decision.decided_at = Some(sim_now().to_rfc3339());
    let denied_decision = decision.clone();

    // Persist updated decision to Turso synchronously.
    if let Err(e) = state.persist_pending_decision(&denied_decision).await {
        tracing::warn!(error = %e, "failed to persist denied decision");
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"id": id, "status": "denied"})),
    )
        .into_response()
}

/// GET /api/tenants/{tenant}/decisions/stream — SSE for new pending decisions.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/decisions/stream"))]
async fn handle_decision_stream(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    _headers: HeaderMap,
) -> impl IntoResponse {
    let rx = state.pending_decision_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(pd) => {
                if pd.tenant != tenant {
                    return None;
                }
                let data = serde_json::to_string(&pd).unwrap_or_default();
                Some(Ok::<Event, Infallible>(
                    Event::default().event("pending_decision").data(data),
                ))
            }
            // Lagged receiver: skip missed events and continue.
            Err(_) => None,
        }
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// GET /api/decisions — list all decisions across all tenants.
#[instrument(skip_all, fields(otel.name = "GET /api/decisions"))]
async fn handle_list_all_decisions(
    State(state): State<ServerState>,
    _headers: HeaderMap,
    Query(params): Query<DecisionListParams>,
) -> impl IntoResponse {
    // Query Turso directly (single source of truth).
    if let Some(turso) = state.turso_opt() {
        match turso.query_all_decisions(params.status.as_deref()).await {
            Ok(data_strings) => {
                let entries: Vec<serde_json::Value> = data_strings
                    .iter()
                    .filter_map(|s| serde_json::from_str(s).ok())
                    .collect();
                let pending_count = entries
                    .iter()
                    .filter(|d| d.get("status").and_then(|v| v.as_str()) == Some("pending"))
                    .count();
                let approved_count = entries
                    .iter()
                    .filter(|d| d.get("status").and_then(|v| v.as_str()) == Some("approved"))
                    .count();
                let denied_count = entries
                    .iter()
                    .filter(|d| d.get("status").and_then(|v| v.as_str()) == Some("denied"))
                    .count();
                let total = entries.len();
                return (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "decisions": entries,
                        "total": total,
                        "pending_count": pending_count,
                        "approved_count": approved_count,
                        "denied_count": denied_count,
                    })),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to query all decisions from Turso");
            }
        }
    }

    // Fallback: empty response.
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "decisions": [],
            "total": 0,
            "pending_count": 0,
            "approved_count": 0,
            "denied_count": 0,
        })),
    )
        .into_response()
}

/// GET /api/decisions/stream — SSE for all pending decisions across all tenants.
#[instrument(skip_all, fields(otel.name = "GET /api/decisions/stream"))]
async fn handle_all_decisions_stream(
    State(state): State<ServerState>,
    _headers: HeaderMap,
) -> impl IntoResponse {
    let rx = state.pending_decision_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| match result {
        Ok(pd) => {
            let data = serde_json::to_string(&pd).unwrap_or_default();
            Some(Ok::<Event, Infallible>(
                Event::default().event("pending_decision").data(data),
            ))
        }
        Err(_) => None,
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ---------------------------------------------------------------------------
// Agent progress SSE endpoint
// ---------------------------------------------------------------------------

/// GET /api/agents/{agent_id}/stream — SSE for agent progress events.
#[instrument(skip_all, fields(agent_id, otel.name = "GET /api/agents/{agent_id}/stream"))]
async fn handle_agent_progress_stream(
    State(state): State<ServerState>,
    Path(agent_id): Path<String>,
    _headers: HeaderMap,
) -> impl IntoResponse {
    let rx = state.agent_progress_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(event) => {
                if event.agent_id != agent_id {
                    return None;
                }
                let data = serde_json::to_string(&event).unwrap_or_default();
                Some(Ok::<Event, Infallible>(
                    Event::default().event(&event.kind).data(data),
                ))
            }
            // Lagged receiver: skip missed events and continue.
            Err(_) => None,
        }
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ---------------------------------------------------------------------------
// Agent authorization + audit endpoints
// ---------------------------------------------------------------------------

/// Request body for POST /api/authorize.
#[derive(serde::Deserialize)]
struct AuthorizeRequest {
    agent_id: String,
    action: String,
    resource_type: String,
    resource_id: String,
    #[serde(default)]
    context: serde_json::Value,
}

/// POST /api/authorize — lightweight Cedar authorization check for agent tool calls.
///
/// Always returns HTTP 200. The agent handles both outcomes programmatically.
/// On deny, creates a `PendingDecision` for human review.
#[instrument(skip_all, fields(otel.name = "POST /api/authorize"))]
async fn handle_authorize(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<AuthorizeRequest>,
) -> impl IntoResponse {
    let security_ctx = security_context_from_headers(&headers, Some(&body.agent_id), None);
    let resource_attrs = std::collections::BTreeMap::new();

    match state.authorize_with_context(
        &security_ctx,
        &body.action,
        &body.resource_type,
        &resource_attrs,
    ) {
        Ok(()) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "allowed": true,
            })),
        )
            .into_response(),
        Err(reason) => {
            let tenant = headers
                .get("x-tenant-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("default");

            let pd = record_authz_denial(
                &state,
                tenant,
                &security_ctx,
                Some(&body.agent_id),
                &body.action,
                &body.resource_type,
                &body.resource_id,
                serde_json::json!({
                    "agent_id": body.agent_id,
                    "context": body.context,
                }),
                &reason,
                None,
                None,
            )
            .await;

            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "allowed": false,
                    "decision_id": pd.id,
                    "reason": reason,
                })),
            )
                .into_response()
        }
    }
}

/// Request body for POST /api/audit.
#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct AuditRequest {
    agent_id: String,
    action: String,
    resource_type: String,
    resource_id: String,
    success: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    duration_ms: Option<u64>,
}

/// POST /api/audit — record a tool invocation in the trajectory log.
#[instrument(skip_all, fields(otel.name = "POST /api/audit"))]
async fn handle_audit(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<AuditRequest>,
) -> impl IntoResponse {
    let tenant = headers
        .get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("default");

    let entry = TrajectoryEntry {
        timestamp: sim_now().to_rfc3339(),
        tenant: tenant.to_string(),
        entity_type: body.resource_type,
        entity_id: body.resource_id,
        action: body.action,
        success: body.success,
        from_status: None,
        to_status: None,
        error: body.error,
        agent_id: Some(body.agent_id),
        session_id: None,
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some(TrajectorySource::Entity),
        spec_governed: Some(false),
    };

    if let Err(e) = state.persist_trajectory_entry(&entry).await {
        tracing::error!(error = %e, "failed to persist audit trajectory entry");
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "recorded": true })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Phase 0: REPL endpoint — exposes the Monty sandbox over HTTP
// ---------------------------------------------------------------------------

/// Request body for POST /api/repl.
#[derive(serde::Deserialize)]
struct ReplRequest {
    code: String,
}

/// POST /api/repl — execute Python code in the Temper Monty sandbox.
///
/// The sandbox provides `temper.*` methods (create, action, submit_specs, etc.)
/// that loop back to this server via HTTP. Agent identity is extracted from
/// `X-Temper-Principal-Id` / `X-Temper-Principal-Kind` / `X-Temper-Agent-Role`
/// headers and forwarded on internal requests.
///
/// Security: 180s timeout, 64MB memory, method allowlisting, no filesystem or
/// network access. External APIs go through `[[integration]]` in IOA specs.
#[instrument(skip_all, fields(otel.name = "POST /api/repl"))]
async fn handle_repl(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<ReplRequest>,
) -> impl IntoResponse {
    let principal_id = headers
        .get("x-temper-principal-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let tenant = headers
        .get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("default")
        .to_string();

    let agent_id = principal_id.clone();
    let port = state.listen_port.get().copied().unwrap_or(4200);
    let code = body.code;

    // The Monty sandbox types are !Send, so we run in a dedicated
    // single-threaded runtime via spawn_blocking.
    let result = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create REPL runtime"); // determinism-ok: one-shot runtime for sandbox
        rt.block_on(async move {
            let config = temper_mcp::repl::ReplConfig {
                server_port: port,
                principal_id,
            };
            temper_mcp::repl::run_repl(&config, &code).await
        })
    })
    .await;

    match result {
        Ok(Ok(result_json)) => {
            let value: serde_json::Value = serde_json::from_str(&result_json)
                .unwrap_or(serde_json::Value::String(result_json));
            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "result": value,
                    "error": serde_json::Value::Null,
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "REPL sandbox execution error");
            // Record sandbox error as trajectory entry (unmet intent).
            let entry = TrajectoryEntry {
                timestamp: sim_now().to_rfc3339(),
                tenant: tenant.clone(),
                entity_type: "sandbox".to_string(),
                entity_id: String::new(),
                action: "repl_execution".to_string(),
                success: false,
                from_status: None,
                to_status: None,
                error: Some(e.to_string()),
                agent_id: agent_id.clone(),
                session_id: None,
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
                source: Some(TrajectorySource::Platform),
                spec_governed: None,
            };
            if let Err(persist_err) = state.persist_trajectory_entry(&entry).await {
                tracing::error!(error = %persist_err, "failed to persist REPL trajectory entry");
            }

            (
                StatusCode::OK,
                axum::Json(serde_json::json!({
                    "result": serde_json::Value::Null,
                    "error": e.to_string(),
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "REPL task panicked");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({
                    "result": serde_json::Value::Null,
                    "error": format!("REPL task panicked: {e}"),
                })),
            )
                .into_response()
        }
    }
}
