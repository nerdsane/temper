//! Authorization check and audit API endpoints.
//!
//! Provides lightweight Cedar authorization checks for agent tool calls and
//! records tool invocations in the trajectory log for observability.

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use crate::authz::{DenialInput, record_authz_denial, require_authenticated_context};
use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};

/// Request body for POST /api/authorize.
#[derive(serde::Deserialize)]
pub(crate) struct AuthorizeRequest {
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
pub(crate) async fn handle_authorize(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::Json(body): axum::Json<AuthorizeRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    let security_ctx = authenticated.security_context();
    if body.agent_id != security_ctx.principal.id {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "agent_id must match the authenticated principal"
            })),
        )
            .into_response();
    }
    let mut resource_attrs = match body.context {
        serde_json::Value::Null => std::collections::BTreeMap::new(),
        serde_json::Value::Object(context) => context
            .into_iter()
            .filter(|(key, _)| !temper_authz::is_cedar_authority_context_key(key))
            .collect(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": "context must be a JSON object"
                })),
            )
                .into_response();
        }
    };
    // The requested resource identity is canonical. Caller-supplied context
    // may enrich the resource, but can never replace the UID Cedar evaluates.
    resource_attrs.insert(
        "id".to_string(),
        serde_json::Value::String(body.resource_id.clone()),
    );
    let tenant = authenticated.tenant();

    match state.authorize_with_context(
        security_ctx,
        &body.action,
        &body.resource_type,
        &resource_attrs,
        tenant.as_str(),
    ) {
        Ok(()) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({
                "allowed": true,
            })),
        )
            .into_response(),
        Err(denial) => {
            let reason = denial.to_string();

            let pd = record_authz_denial(
                &state,
                DenialInput {
                    tenant: tenant.as_str(),
                    security_ctx,
                    agent_id_override: None,
                    action: &body.action,
                    resource_type: &body.resource_type,
                    resource_id: &body.resource_id,
                    resource_attrs: serde_json::Value::Object(
                        resource_attrs.clone().into_iter().collect(),
                    ),
                    reason: &reason,
                    module_name: None,
                    from_status: None,
                    intent: authenticated.intent().map(str::to_string),
                    session_id: authenticated.session_id().map(str::to_string),
                    // Pre-flight probe: action, resource type, and session are
                    // caller-chosen, so this row must never enter a conformance verdict.
                    spec_governed: Some(false),
                },
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
pub(crate) struct AuditRequest {
    agent_id: String,
    action: String,
    resource_type: String,
    resource_id: String,
    success: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    request_body: Option<serde_json::Value>,
    #[serde(default)]
    intent: Option<String>,
    /// Tool result summary (accepted for forward compatibility).
    #[serde(default)]
    #[allow(dead_code)]
    result: Option<String>,
    /// Execution duration in milliseconds (accepted for forward compatibility).
    #[serde(default)]
    #[allow(dead_code)]
    duration_ms: Option<u64>,
}

/// POST /api/audit — record a tool invocation in the trajectory log.
#[instrument(skip_all, fields(otel.name = "POST /api/audit"))]
pub(crate) async fn handle_audit(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::Json(body): axum::Json<AuditRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    let security_ctx = authenticated.security_context();
    if body.agent_id != security_ctx.principal.id {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "agent_id must match the authenticated principal"
            })),
        )
            .into_response();
    }
    let tenant = authenticated.tenant();

    let entry = TrajectoryEntry {
        timestamp: sim_now().to_rfc3339(),
        tenant: tenant.as_str().to_string(),
        entity_type: body.resource_type,
        entity_id: body.resource_id,
        action: body.action,
        success: body.success,
        from_status: None,
        to_status: None,
        error: body.error,
        agent_id: Some(security_ctx.principal.id.clone()),
        session_id: body.session_id,
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some(TrajectorySource::Entity),
        spec_governed: Some(false),
        agent_type: security_ctx.principal.agent_type.clone(),
        request_body: body.request_body,
        intent: body.intent,
        matched_policy_ids: None,
        capture_seq: None,
    };

    if !state.enqueue_trajectory_entry(entry) {
        tracing::warn!("failed to enqueue audit trajectory entry");
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({ "recorded": true })),
    )
        .into_response()
}
