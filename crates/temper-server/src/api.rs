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

use crate::authz_helpers::{record_authz_denial, security_context_from_headers};
use crate::state::{DecisionStatus, PolicyScope, ServerState};
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
        // Cross-tenant decision endpoints
        .route("/decisions", get(handle_list_all_decisions))
        .route("/decisions/stream", get(handle_all_decisions_stream))
}

fn is_cross_tenant_decision_admin(headers: &HeaderMap) -> bool {
    let security_ctx = security_context_from_headers(headers, None, None);
    matches!(security_ctx.principal.kind, PrincipalKind::Admin)
}

fn cross_tenant_admin_denied() -> axum::response::Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": {
                "code": "AuthorizationDenied",
                "message": "Admin principal required for cross-tenant decision access",
            }
        })),
    )
        .into_response()
}

fn authorize_tenant_decision_management(
    state: &ServerState,
    headers: &HeaderMap,
    tenant: &str,
) -> Option<axum::response::Response> {
    let security_ctx = security_context_from_headers(headers, None, None);
    if state.authz.policy_count() == 0
        && matches!(security_ctx.principal.kind, PrincipalKind::Admin)
    {
        // Bootstrap path: before any Cedar policies exist, allow explicit admins
        // to manage pending decisions so governance cannot deadlock.
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
        );
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

/// PUT /api/tenants/{tenant}/secrets/{key_name} — encrypt and store a secret.
async fn handle_put_secret(
    State(state): State<ServerState>,
    Path((tenant, key_name)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let Some(vault) = state.secrets_vault.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Secrets vault not configured (missing TEMPER_VAULT_KEY)",
        )
            .into_response();
    };

    let body_json: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")).into_response();
        }
    };

    let value = match body_json.get("value").and_then(|v| v.as_str()) {
        Some(v) => v,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "Missing \'value\' field in request body",
            )
                .into_response();
        }
    };

    if value.len() > crate::secrets_vault::MAX_SECRET_VALUE_BYTES {
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encryption failed: {e}"),
            )
                .into_response();
        }
    };

    // Cache in memory.
    if let Err(e) = vault.cache_secret(&tenant, &key_name, value.to_string()) {
        return (StatusCode::CONFLICT, e).into_response();
    }

    // Persist to DB.
    if let Err(e) = state
        .upsert_secret(&tenant, &key_name, &ciphertext, &nonce)
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Persistence failed: {e}"),
        )
            .into_response();
    }

    StatusCode::NO_CONTENT.into_response()
}

/// DELETE /api/tenants/{tenant}/secrets/{key_name} — remove a secret.
async fn handle_delete_secret(
    State(state): State<ServerState>,
    Path((tenant, key_name)): Path<(String, String)>,
) -> impl IntoResponse {
    let Some(vault) = state.secrets_vault.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Secrets vault not configured",
        )
            .into_response();
    };

    vault.remove_secret(&tenant, &key_name);

    match state.delete_secret(&tenant, &key_name).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Delete failed: {e}"),
        )
            .into_response(),
    }
}

/// GET /api/tenants/{tenant}/secrets — list secret key names (never values).
async fn handle_list_secrets(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
) -> impl IntoResponse {
    let Some(vault) = state.secrets_vault.as_ref() else {
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
        );
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
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")).into_response();
        }
    };

    let policy_text = match body_json.get("policy_text").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => {
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
        );
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
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {e}")).into_response();
        }
    };

    let rule = match body_json.get("rule").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => {
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
async fn handle_list_decisions(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
    Query(params): Query<DecisionListParams>,
) -> impl IntoResponse {
    if let Some(resp) = authorize_tenant_decision_management(&state, &headers, &tenant) {
        return resp;
    }

    let log = state.pending_decision_log.read().unwrap(); // ci-ok: infallible lock
    let entries: Vec<_> = log
        .entries()
        .iter()
        .filter(|d| d.tenant == tenant)
        .filter(|d| {
            if let Some(ref s) = params.status {
                let status_str = serde_json::to_value(&d.status)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                status_str == *s
            } else {
                true
            }
        })
        .cloned()
        .collect();

    let pending_count = entries
        .iter()
        .filter(|d| d.status == DecisionStatus::Pending)
        .count();
    let approved_count = entries
        .iter()
        .filter(|d| d.status == DecisionStatus::Approved)
        .count();
    let denied_count = entries
        .iter()
        .filter(|d| d.status == DecisionStatus::Denied)
        .count();
    let total = entries.len();

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "decisions": entries,
            "total": total,
            "pending_count": pending_count,
            "approved_count": approved_count,
            "denied_count": denied_count,
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
async fn handle_approve_decision(
    State(state): State<ServerState>,
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<ApproveBody>,
) -> impl IntoResponse {
    if let Some(resp) = authorize_tenant_decision_management(&state, &headers, &tenant) {
        return resp;
    }

    let scope: PolicyScope = match body.scope.as_str() {
        "narrow" => PolicyScope::Narrow,
        "medium" => PolicyScope::Medium,
        "broad" => PolicyScope::Broad,
        other => {
            return (
                StatusCode::BAD_REQUEST,
                format!("Invalid scope: {other}. Must be narrow, medium, or broad."),
            )
                .into_response();
        }
    };

    // Validate decision + stage generated policy.
    let mut log = state.pending_decision_log.write().unwrap(); // ci-ok: infallible lock
    let decision = match log.get_mut(&id) {
        Some(d) if d.tenant == tenant => d,
        _ => {
            return (StatusCode::NOT_FOUND, "Decision not found").into_response();
        }
    };

    if decision.status != DecisionStatus::Pending {
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to reload policies: {e}"),
            )
                .into_response();
        }

        *policies = next_policies;
    }

    // Mark decision approved only after policy reload succeeds.
    decision.status = DecisionStatus::Approved;
    decision.approved_scope = Some(scope);
    decision.generated_policy = Some(generated_policy.clone());
    decision.decided_by = body.decided_by.clone();
    decision.decided_at = Some(sim_now().to_rfc3339());
    drop(log);

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
    state.record_store.insert_decision(d_record);

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
async fn handle_deny_decision(
    State(state): State<ServerState>,
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<axum::Json<serde_json::Value>>,
) -> impl IntoResponse {
    if let Some(resp) = authorize_tenant_decision_management(&state, &headers, &tenant) {
        return resp;
    }

    let decided_by = body
        .as_ref()
        .and_then(|b| b.get("decided_by"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let mut log = state.pending_decision_log.write().unwrap(); // ci-ok: infallible lock
    let decision = match log.get_mut(&id) {
        Some(d) if d.tenant == tenant => d,
        _ => {
            return (StatusCode::NOT_FOUND, "Decision not found").into_response();
        }
    };

    if decision.status != DecisionStatus::Pending {
        return (
            StatusCode::CONFLICT,
            format!("Decision already resolved as {:?}", decision.status),
        )
            .into_response();
    }

    decision.status = DecisionStatus::Denied;
    decision.decided_by = decided_by;
    decision.decided_at = Some(sim_now().to_rfc3339());

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"id": id, "status": "denied"})),
    )
        .into_response()
}

/// GET /api/tenants/{tenant}/decisions/stream — SSE for new pending decisions.
async fn handle_decision_stream(
    State(state): State<ServerState>,
    Path(tenant): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(resp) = authorize_tenant_decision_management(&state, &headers, &tenant) {
        return resp;
    }

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
async fn handle_list_all_decisions(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(params): Query<DecisionListParams>,
) -> impl IntoResponse {
    if !is_cross_tenant_decision_admin(&headers) {
        return cross_tenant_admin_denied();
    }

    let log = state.pending_decision_log.read().unwrap(); // ci-ok: infallible lock
    let entries: Vec<_> = log
        .entries()
        .iter()
        .filter(|d| {
            if let Some(ref s) = params.status {
                let status_str = serde_json::to_value(&d.status)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default();
                status_str == *s
            } else {
                true
            }
        })
        .cloned()
        .collect();

    let pending_count = entries
        .iter()
        .filter(|d| d.status == DecisionStatus::Pending)
        .count();
    let approved_count = entries
        .iter()
        .filter(|d| d.status == DecisionStatus::Approved)
        .count();
    let denied_count = entries
        .iter()
        .filter(|d| d.status == DecisionStatus::Denied)
        .count();
    let total = entries.len();

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "decisions": entries,
            "total": total,
            "pending_count": pending_count,
            "approved_count": approved_count,
            "denied_count": denied_count,
        })),
    )
        .into_response()
}

/// GET /api/decisions/stream — SSE for all pending decisions across all tenants.
async fn handle_all_decisions_stream(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !is_cross_tenant_decision_admin(&headers) {
        return cross_tenant_admin_denied();
    }

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
