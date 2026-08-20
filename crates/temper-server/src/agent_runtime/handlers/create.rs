//! Agent-run creation handler.

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::scheduler::sim_uuid;

use crate::agent_runtime::models::{CreateRunRequest, CreateRunResponse};
use crate::state::ServerState;

use super::common::{caller_agent_context, error_response, require_auth};

/// Create a new agent run: create entity → configure → provision.
#[tracing::instrument(
    skip_all,
    fields(
        otel.name = "agent.run.create",
        agent.run_id = tracing::field::Empty,
        agent.provider = tracing::field::Empty,
        agent.model = tracing::field::Empty,
    )
)]
pub(super) async fn create_run(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Json(req): Json<CreateRunRequest>,
) -> Response {
    let (tenant, authenticated) = match require_auth(authenticated) {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let run_id = format!("run_{}", sim_uuid().simple());

    tracing::Span::current().record("agent.run_id", &run_id);
    tracing::Span::current().record("agent.provider", &req.sandbox_provider);
    tracing::Span::current().record("agent.model", &req.model);

    let agent_ctx = caller_agent_context(&authenticated);
    let tools_str = req.tools.join(",");
    let max_turns = req
        .budget
        .as_ref()
        .map(|b| b.max_turns.clone())
        .unwrap_or_else(|| req.max_turns.clone());
    let heartbeat_timeout_seconds = req
        .budget
        .as_ref()
        .map(|b| b.timeout_seconds.to_string())
        .unwrap_or_default();

    let configure_params = json!({
        "system_prompt": req.system_prompt.unwrap_or_else(|| {
            "You are a coding agent. Fix failing tests and show the diff.".to_string()
        }),
        "user_message": req.prompt,
        "model": req.model,
        "provider": req.provider,
        "tools_enabled": tools_str,
        "sandbox_url": req.sandbox_url,
        "sandbox_provider": req.sandbox_provider,
        "sandbox_image": req.sandbox_image.unwrap_or_default(),
        "repo_url": req.repo.as_ref().map(|r| r.url.clone()).unwrap_or_default(),
        "repo_ref": req.repo.as_ref().map(|r| r.r#ref.clone()).unwrap_or_default(),
        "workdir": req.workdir,
        "max_turns": max_turns,
        "heartbeat_timeout_seconds": heartbeat_timeout_seconds,
    });

    let configure_result = state
        .dispatch_tenant_action(
            &tenant,
            "TemperAgent",
            &run_id,
            "Configure",
            configure_params,
            &agent_ctx,
        )
        .await;

    if let Err(e) = &configure_result {
        tracing::warn!(error = %e, "AgentRuntime: Configure failed");
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if let Ok(resp) = &configure_result
        && !resp.success
    {
        let msg = resp.error.as_deref().unwrap_or("configure failed");
        return error_response(StatusCode::BAD_REQUEST, msg);
    }

    let provision_result = state
        .dispatch_tenant_action(
            &tenant,
            "TemperAgent",
            &run_id,
            "Provision",
            json!({}),
            &agent_ctx,
        )
        .await;

    if let Err(e) = &provision_result {
        tracing::warn!(error = %e, "AgentRuntime: Provision failed");
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if let Ok(resp) = &provision_result
        && !resp.success
    {
        let msg = resp.error.as_deref().unwrap_or("provision failed");
        return error_response(StatusCode::BAD_REQUEST, msg);
    }

    let entity_state = state
        .get_tenant_entity_state(&tenant, "TemperAgent", &run_id)
        .await;

    let status = entity_state
        .as_ref()
        .map(|s| s.state.status.clone())
        .unwrap_or_else(|_| "Provisioning".to_string());

    (
        StatusCode::ACCEPTED,
        [(CONTENT_TYPE, "application/json")],
        Json(CreateRunResponse { run_id, status }),
    )
        .into_response()
}
