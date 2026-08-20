//! Agent-run steering and cancellation handlers.

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use temper_authz::AuthenticatedRequestContext;

use crate::agent_runtime::models::{CancelResponse, SteerRequest};
use crate::state::ServerState;

use super::common::{caller_agent_context, error_response, require_auth};

/// Steer an active agent run by queuing a steering message.
#[tracing::instrument(
    skip_all,
    fields(
        otel.name = "agent.run.steer",
        agent.run_id = %id,
    )
)]
pub(super) async fn steer_run(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(id): Path<String>,
    Json(req): Json<SteerRequest>,
) -> Response {
    let (tenant, authenticated) = match require_auth(authenticated) {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let agent_ctx = caller_agent_context(&authenticated);
    let steering_messages = json!([req.message]);

    let result = state
        .dispatch_tenant_action(
            &tenant,
            "TemperAgent",
            &id,
            "Steer",
            json!({ "steering_messages": steering_messages }),
            &agent_ctx,
        )
        .await;

    match result {
        Ok(resp) if resp.success => (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            Json(json!({ "run_id": id, "status": resp.state.status })),
        )
            .into_response(),
        Ok(resp) => {
            let msg = resp.error.as_deref().unwrap_or("steer failed");
            error_response(StatusCode::BAD_REQUEST, msg)
        }
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// Cancel an agent run.
#[tracing::instrument(
    skip_all,
    fields(
        otel.name = "agent.run.cancel",
        agent.run_id = %id,
    )
)]
pub(super) async fn cancel_run(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(id): Path<String>,
) -> Response {
    let (tenant, authenticated) = match require_auth(authenticated) {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let agent_ctx = caller_agent_context(&authenticated);

    let result = state
        .dispatch_tenant_action(&tenant, "TemperAgent", &id, "Cancel", json!({}), &agent_ctx)
        .await;

    match result {
        Ok(resp) if resp.success => (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            Json(CancelResponse {
                run_id: id,
                status: resp.state.status,
            }),
        )
            .into_response(),
        Ok(resp) => {
            let msg = resp.error.as_deref().unwrap_or("cancel failed");
            error_response(StatusCode::BAD_REQUEST, msg)
        }
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}
