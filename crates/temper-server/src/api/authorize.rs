//! Authorization check and audit API endpoints.
//!
//! Provides lightweight Cedar authorization checks for agent tool calls and
//! records tool invocations in the trajectory log for observability.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use crate::authz::{DenialInput, record_authz_denial, security_context_from_headers};
use crate::odata::extract_tenant;
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
    headers: HeaderMap,
    axum::Json(body): axum::Json<AuthorizeRequest>,
) -> impl IntoResponse {
    let security_ctx = security_context_from_headers(&headers, Some(&body.agent_id), None, None);
    let resource_attrs = std::collections::BTreeMap::new();
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };

    match state.authorize_with_context(
        &security_ctx,
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
                    security_ctx: &security_ctx,
                    agent_id_override: Some(&body.agent_id),
                    action: &body.action,
                    resource_type: &body.resource_type,
                    resource_id: &body.resource_id,
                    resource_attrs: serde_json::json!({
                        "agent_id": body.agent_id,
                        "context": body.context,
                    }),
                    reason: &reason,
                    module_name: None,
                    from_status: None,
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
    headers: HeaderMap,
    axum::Json(body): axum::Json<AuditRequest>,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };

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
        agent_id: Some(body.agent_id),
        session_id: body.session_id,
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some(TrajectorySource::Entity),
        spec_governed: Some(false),
        agent_type: None,
        request_body: body.request_body,
        intent: body.intent,
        matched_policy_ids: None,
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
