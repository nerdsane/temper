//! Authorization questions and audit API endpoints.

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use crate::authz::require_authenticated_context;
use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};

/// One record snapshot, shared by all its requested moves.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizeRecord {
    resource_type: String,
    resource_id: String,
    actions: Vec<String>,
    #[serde(default)]
    context: serde_json::Value,
}

/// Batched questions for one credential-derived principal, in request order.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthorizeBatch {
    agent_id: String,
    records: Vec<AuthorizeRecord>,
}

/// Single-pair compatibility for the published Rust and TypeScript SDKs.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthorizeSingle {
    agent_id: String,
    action: String,
    resource_type: String,
    resource_id: String,
    #[serde(default)]
    context: serde_json::Value,
}

/// Request body for POST /api/authorize.
#[derive(serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum AuthorizeRequest {
    Batch(AuthorizeBatch),
    Single(AuthorizeSingle),
}

/// Ask Cedar without requesting a move. A question is a read and writes
/// nothing: no decision, denial pattern or trajectory row, however many
/// records and moves a page asks about. Each answer is counted as
/// `check:offer` in `temper_cedar_evaluations_total`, apart from the
/// `check:gate` refusals of requests, and stays visible as the authz wide
/// event every check emits.
///
/// Each record's caller-supplied snapshot is decoded once and shared across its
/// actions, exactly as for the original single check. Cost: one Cedar evaluation
/// per pair, zero store reads and zero writes.
/// There is no item-count cap: the router's request-body byte limit bounds input.
/// These are offers against a snapshot, not grants; dispatch authorizes again.
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
    let (agent_id, records, single) = match body {
        AuthorizeRequest::Batch(batch) => (batch.agent_id, batch.records, false),
        AuthorizeRequest::Single(one) => (
            one.agent_id,
            vec![AuthorizeRecord {
                resource_type: one.resource_type,
                resource_id: one.resource_id,
                actions: vec![one.action],
                context: one.context,
            }],
            true,
        ),
    };
    let security_ctx = authenticated.security_context();
    if agent_id != security_ctx.principal.id {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "agent_id must match the authenticated principal"
            })),
        )
            .into_response();
    }
    // Validate the whole page before evaluating or emitting any trajectory.
    if records
        .iter()
        .any(|r| !r.context.is_null() && !r.context.is_object())
    {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": "context must be a JSON object"
            })),
        )
            .into_response();
    }
    let tenant = authenticated.tenant();
    let mut answers = Vec::with_capacity(records.len());
    for record in records {
        let mut attrs: std::collections::BTreeMap<String, serde_json::Value> = match record.context
        {
            serde_json::Value::Object(context) => context.into_iter().collect(),
            _ => Default::default(),
        };
        // Caller context cannot replace the canonical Cedar UID.
        attrs.insert("id".to_string(), serde_json::json!(record.resource_id));
        let mut actions = Vec::with_capacity(record.actions.len());
        for action in record.actions {
            let reason = state
                .authorize_offer_with_context(
                    security_ctx,
                    &action,
                    &record.resource_type,
                    &attrs,
                    tenant.as_str(),
                )
                .err()
                .map(|denial| denial.to_string());
            actions.push(serde_json::json!({"action": action, "allowed": reason.is_none(), "reason": reason}));
        }
        answers.push(serde_json::json!({
            "resource_type": record.resource_type, "resource_id": record.resource_id, "actions": actions
        }));
    }
    let answer = if single {
        let mut answer = answers.remove(0)["actions"][0].clone();
        answer
            .as_object_mut()
            .expect("single answer is an object")
            .remove("action");
        answer
    } else {
        serde_json::json!({"records": answers})
    };
    (StatusCode::OK, axum::Json(answer)).into_response()
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
