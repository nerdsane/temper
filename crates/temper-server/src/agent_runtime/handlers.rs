//! HTTP handlers for the Agent Runtime API.
//!
//! These handlers are thin wrappers that translate clean REST requests
//! into Temper IOA action dispatches against the `TemperAgent` entity.
//! They call `ServerState::dispatch_tenant_action` directly — no
//! self-referential HTTP round-trips.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use opentelemetry::trace::TraceContextExt;
use serde_json::json;
use temper_authz::AuthenticatedRequestContext;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use temper_runtime::tenant::TenantId;
use uuid::Uuid;

use crate::request_context::AgentContext;
use crate::state::ServerState;

use super::models::{
    CancelResponse, CreateRunRequest, CreateRunResponse, ErrorResponse, RunStatus, SteerRequest,
};

/// Build the `/v1/agent-runs` router.
pub fn build_agent_runtime_router() -> axum::Router<ServerState> {
    axum::Router::new()
        .route("/agent-runs", post(create_run))
        .route("/agent-runs/{id}", get(get_run))
        .route("/agent-runs/{id}/steer", post(steer_run))
        .route("/agent-runs/{id}/cancel", post(cancel_run))
}

/// Resolve the tenant and authenticated context from the request.
/// Falls back to the `X-Tenant-Id` header and an admin security context
/// when no credential was resolved (local dev without TEMPER_API_KEY).
fn resolve_auth(
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: &axum::http::HeaderMap,
) -> Result<(TenantId, AuthenticatedRequestContext), Response> {
    if let Some(Extension(ctx)) = authenticated {
        return Ok((ctx.tenant().clone(), ctx));
    }

    // Fallback: resolve tenant from header, create an admin context.
    let tenant = headers
        .get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| TenantId::try_new(s).unwrap_or_default())
        .unwrap_or_default();

    let security_ctx = temper_authz::SecurityContext::from_resolved_identity(
        "admin",
        "admin",
        None,
    );
    let ctx = AuthenticatedRequestContext::new(tenant.clone(), security_ctx);
    Ok((tenant, ctx))
}

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
async fn create_run(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CreateRunRequest>,
) -> Response {
    let (tenant, _authenticated) = match resolve_auth(authenticated, &headers) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let run_id = format!("run_{}", Uuid::new_v4().simple());

    tracing::Span::current().record("agent.run_id", &run_id);
    tracing::Span::current().record("agent.provider", &req.sandbox_provider);
    tracing::Span::current().record("agent.model", &req.model);

    let agent_ctx = AgentContext::for_service("agent-runtime-api");

    // 1. Create the TemperAgent entity (implicit on first action).
    //    We call Configure first to set up all parameters.
    let tools_str = req.tools.join(",");
    let max_turns = req
        .budget
        .as_ref()
        .map(|b| b.max_turns.clone())
        .unwrap_or_else(|| req.max_turns.clone());

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
        "workdir": req.workdir,
        "max_turns": max_turns,
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
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    if let Ok(resp) = &configure_result {
        if !resp.success {
            let msg = resp.error.as_deref().unwrap_or("configure failed");
            return error_response(StatusCode::BAD_REQUEST, msg);
        }
    }

    // 2. Provision — triggers sandbox_provisioner WASM → SandboxReady callback.
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
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    if let Ok(resp) = &provision_result {
        if !resp.success {
            let msg = resp.error.as_deref().unwrap_or("provision failed");
            return error_response(StatusCode::BAD_REQUEST, msg);
        }
    }

    // 3. Read the current state to return the status.
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
        Json(CreateRunResponse {
            run_id,
            status,
        }),
    )
        .into_response()
}

/// Get the status of an agent run.
#[tracing::instrument(
    skip_all,
    fields(
        otel.name = "agent.run.get",
        agent.run_id = %id,
    )
)]
async fn get_run(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (tenant, _authenticated) = match resolve_auth(authenticated, &headers) {
        Ok(t) => t,
        Err(r) => return r,
    };

    let entity_state = match state
        .get_tenant_entity_state(&tenant, "TemperAgent", &id)
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            return error_response(StatusCode::NOT_FOUND, &format!("Entity not found: {e}"));
        }
    };

    let s = &entity_state.state;
    let fields = &s.fields;

    // Extract trace ID from the current span context for correlation.
    let trace_id = {
        let span_ctx = tracing::Span::current().context();
        let span = span_ctx.span();
        let span_ctx = span.span_context();
        if span_ctx.is_valid() {
            Some(span_ctx.trace_id().to_string())
        } else {
            None
        }
    };

    let turn = s
        .counters
        .get("turn_count")
        .copied()
        .unwrap_or(0) as u64;

    let sandbox_id = fields
        .get("sandbox_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "static-sandbox")
        .map(String::from);

    let checkpoint_ref = fields
        .get("workspace_checkpoint")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    let error = fields
        .get("error_message")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    let result = fields
        .get("result")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

    (
        StatusCode::OK,
        [(CONTENT_TYPE, "application/json")],
        Json(RunStatus {
            run_id: id,
            status: s.status.clone(),
            turn,
            sandbox_id,
            checkpoint_ref,
            trace_id,
            error,
            result,
        }),
    )
        .into_response()
}

/// Steer an active agent run by queuing a steering message.
#[tracing::instrument(
    skip_all,
    fields(
        otel.name = "agent.run.steer",
        agent.run_id = %id,
    )
)]
async fn steer_run(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<SteerRequest>,
) -> Response {
    let (tenant, _authenticated) = match resolve_auth(authenticated, &headers) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let agent_ctx = AgentContext::for_service("agent-runtime-api");

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
async fn cancel_run(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let (tenant, _authenticated) = match resolve_auth(authenticated, &headers) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let agent_ctx = AgentContext::for_service("agent-runtime-api");

    let result = state
        .dispatch_tenant_action(
            &tenant,
            "TemperAgent",
            &id,
            "Cancel",
            json!({}),
            &agent_ctx,
        )
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

/// Build a JSON error response.
fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(CONTENT_TYPE, "application/json")],
        Json(ErrorResponse {
            error: message.to_string(),
        }),
    )
        .into_response()
}
