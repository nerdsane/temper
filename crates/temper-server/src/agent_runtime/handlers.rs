//! HTTP handlers for the Agent Runtime API.
//!
//! These handlers are thin wrappers that translate clean REST requests
//! into Temper IOA action dispatches against the `TemperAgent` entity.
//! They call `ServerState::dispatch_tenant_action` directly — no
//! self-referential HTTP round-trips.

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use opentelemetry::trace::TraceContextExt;
use serde_json::json;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::tenant::TenantId;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

use crate::request_context::AgentContext;
use crate::state::ServerState;

use super::models::{
    CancelResponse, CreateRunRequest, CreateRunResponse, DeleteRunResponse, ErrorResponse,
    RunStatus, SteerRequest,
};

/// Build the `/v1/agent-runs` router.
pub fn build_agent_runtime_router() -> axum::Router<ServerState> {
    axum::Router::new()
        .route("/agent-runs", post(create_run))
        .route("/agent-runs/{id}", get(get_run).delete(delete_run))
        .route("/agent-runs/{id}/steer", post(steer_run))
        .route("/agent-runs/{id}/cancel", post(cancel_run))
}

/// Require typed authenticated authority resolved by the platform bearer edge.
///
/// Identity is never reconstructed from request headers: the platform's
/// `bearer_auth_check` resolves `Authorization: Bearer <token>` against the
/// tenant's `AgentCredential` registry and attaches the typed context. A
/// missing context means the caller presented no resolvable credential.
fn require_auth(
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> Result<(TenantId, AuthenticatedRequestContext), Response> {
    match authenticated {
        Some(Extension(ctx)) => Ok((ctx.tenant().clone(), ctx)),
        None => Err(error_response(
            StatusCode::UNAUTHORIZED,
            "a valid tenant credential is required (Authorization: Bearer <token>)",
        )),
    }
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
    Json(req): Json<CreateRunRequest>,
) -> Response {
    let (tenant, authenticated) = match require_auth(authenticated) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let run_id = format!("run_{}", Uuid::new_v4().simple());

    tracing::Span::current().record("agent.run_id", &run_id);
    tracing::Span::current().record("agent.provider", &req.sandbox_provider);
    tracing::Span::current().record("agent.model", &req.model);

    // Carry the caller's exact typed authority into dispatch so Cedar
    // evaluates the real principal (never a broad service identity).
    let agent_ctx = caller_agent_context(&authenticated);

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
        "repo_url": req.repo.as_ref().map(|r| r.url.clone()).unwrap_or_default(),
        "repo_ref": req.repo.as_ref().map(|r| r.r#ref.clone()).unwrap_or_default(),
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
        Json(CreateRunResponse { run_id, status }),
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
    Path(id): Path<String>,
) -> Response {
    let (tenant, _authenticated) = match require_auth(authenticated) {
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
    if s.status == "Deleted" {
        return error_response(StatusCode::NOT_FOUND, "agent run has been deleted");
    }
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

    let turn = s.counters.get("turn_count").copied().unwrap_or(0) as u64;

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
    Path(id): Path<String>,
    Json(req): Json<SteerRequest>,
) -> Response {
    let (tenant, authenticated) = match require_auth(authenticated) {
        Ok(t) => t,
        Err(r) => return r,
    };
    // Carry the caller's exact typed authority into dispatch so Cedar
    // evaluates the real principal (never a broad service identity).
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
async fn cancel_run(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(id): Path<String>,
) -> Response {
    let (tenant, authenticated) = match require_auth(authenticated) {
        Ok(t) => t,
        Err(r) => return r,
    };
    // Carry the caller's exact typed authority into dispatch so Cedar
    // evaluates the real principal (never a broad service identity).
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

/// Delete a terminal agent run after its sandbox teardown succeeds.
///
/// Deletion is an asynchronous, teardown-gated lifecycle. A request moves a
/// terminal run into `Deleting`, where `sandbox_destroyer` removes its provider
/// sandbox. The WASM callback advances it to `Deleted` only after a successful
/// teardown; failures become `DeletionFailed` and can be retried by repeating
/// this request. The entity remains event-sourced for audit but normal reads
/// return 404 after it reaches `Deleted`.
#[tracing::instrument(
    skip_all,
    fields(
        otel.name = "agent.run.delete",
        agent.run_id = %id,
    )
)]
async fn delete_run(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(id): Path<String>,
) -> Response {
    let (tenant, authenticated) = match require_auth(authenticated) {
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

    let action = match deletion_disposition(&entity_state.state.status) {
        DeletionDisposition::Dispatch(action) => action,
        DeletionDisposition::Deleted => {
            return authorize_deleted_delete(
                &state,
                &tenant,
                &id,
                &entity_state.state.status,
                &entity_state.state.fields,
                &authenticated,
            )
            .await;
        }
        DeletionDisposition::Active => {
            return error_response(
                StatusCode::CONFLICT,
                "only terminal agent runs can be deleted; cancel an active run first",
            );
        }
    };

    let agent_ctx = caller_agent_context(&authenticated);
    let result = state
        .dispatch_tenant_action(&tenant, "TemperAgent", &id, action, json!({}), &agent_ctx)
        .await;

    match result {
        Ok(resp) if resp.success => deletion_accepted_response(id, resp.state.status),
        Ok(resp) => {
            let message = resp
                .error
                .as_deref()
                .unwrap_or("agent-run deletion was rejected");
            deletion_race_response(&state, &tenant, &id, message).await
        }
        Err(error) => deletion_race_response(&state, &tenant, &id, &error).await,
    }
}

/// Authorize an idempotent response for a logically deleted run without
/// creating an outgoing transition from the terminal `Deleted` state.
async fn authorize_deleted_delete(
    state: &ServerState,
    tenant: &TenantId,
    run_id: &str,
    status: &str,
    fields: &serde_json::Value,
    authenticated: &AuthenticatedRequestContext,
) -> Response {
    let resource_attrs = match state
        .build_authz_resource_attrs(tenant, "TemperAgent", run_id, status, fields)
        .await
    {
        Ok(attrs) => attrs,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error),
    };
    match state.authorize_with_context(
        &authenticated.security_context(),
        "RequestDeletion",
        "TemperAgent",
        &resource_attrs,
        tenant.as_str(),
    ) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(denial) => error_response(StatusCode::FORBIDDEN, &denial.to_string()),
    }
}

/// Return the public asynchronous-deletion response.
fn deletion_accepted_response(run_id: String, status: String) -> Response {
    if status == "Deleted" {
        return StatusCode::NO_CONTENT.into_response();
    }
    (
        StatusCode::ACCEPTED,
        [(CONTENT_TYPE, "application/json")],
        Json(DeleteRunResponse { run_id, status }),
    )
        .into_response()
}

/// Resolve a competing DELETE request without exposing a spurious conflict.
///
/// Two callers can both read a terminal state before one dispatches
/// `RequestDeletion`. When the second dispatch loses that race, reread the
/// authoritative actor state: `Deleting` is a successful idempotent outcome,
/// and `Deleted` is a successful no-op. Other states retain the original
/// rejection so authorization and lifecycle errors are never masked.
async fn deletion_race_response(
    state: &ServerState,
    tenant: &TenantId,
    run_id: &str,
    original_error: &str,
) -> Response {
    if !is_stale_deletion_dispatch_error(original_error) {
        return error_response(StatusCode::BAD_REQUEST, original_error);
    }

    match state
        .get_tenant_entity_state(tenant, "TemperAgent", run_id)
        .await
    {
        Ok(current) => match current.state.status.as_str() {
            "Deleting" => deletion_accepted_response(run_id.to_string(), "Deleting".to_string()),
            "Deleted" => StatusCode::NO_CONTENT.into_response(),
            // A background teardown can fail before a concurrent caller reads
            // the new state. Surface that retryable lifecycle state instead
            // of returning a misleading transition conflict.
            "DeletionFailed" => {
                deletion_accepted_response(run_id.to_string(), "DeletionFailed".to_string())
            }
            _ => error_response(StatusCode::BAD_REQUEST, original_error),
        },
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, original_error),
    }
}

/// Whether a failed dispatch can only be explained by a stale actor state.
///
/// Do not reread-and-convert arbitrary failures into idempotent success: doing
/// so can hide Cedar authorization denials from the original caller.
fn is_stale_deletion_dispatch_error(error: &str) -> bool {
    error.contains("not valid from state")
        || error.contains("authorization became stale; retry against current state")
}

/// The public deletion behavior for a lifecycle status.
#[derive(Debug, PartialEq, Eq)]
enum DeletionDisposition {
    /// Dispatch this Cedar-governed action before starting teardown.
    Dispatch(&'static str),
    /// Logical deletion is terminal; authorize a no-content response directly.
    Deleted,
    /// Work is still active and must be cancelled first.
    Active,
}

/// Select the governed deletion behavior for the current lifecycle state.
fn deletion_disposition(status: &str) -> DeletionDisposition {
    match status {
        "Completed" | "Failed" | "Cancelled" => DeletionDisposition::Dispatch("RequestDeletion"),
        "DeletionFailed" | "Deleting" => DeletionDisposition::Dispatch("RetryDeletion"),
        "Deleted" => DeletionDisposition::Deleted,
        _ => DeletionDisposition::Active,
    }
}

/// Build a dispatch context that carries the caller's exact authority.
///
/// Mirrors the OData write path: the security context is attached verbatim and
/// agent identity fields are copied only for Agent/Admin principals. No field
/// is reconstructed from request headers.
fn caller_agent_context(authenticated: &AuthenticatedRequestContext) -> AgentContext {
    let security_context = authenticated.security_context();
    let mut agent_ctx = AgentContext::default();
    agent_ctx.security_ctx = Some(security_context.clone());
    if matches!(
        security_context.principal.kind,
        temper_authz::PrincipalKind::Agent | temper_authz::PrincipalKind::Admin
    ) {
        agent_ctx.agent_id = Some(security_context.principal.id.clone());
        agent_ctx.agent_type = security_context.principal.agent_type.clone();
    }
    agent_ctx
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

#[cfg(test)]
mod tests {
    use super::{DeletionDisposition, deletion_disposition, is_stale_deletion_dispatch_error};

    #[test]
    fn deletion_starts_only_from_terminal_states() {
        for status in ["Completed", "Failed", "Cancelled"] {
            assert_eq!(
                deletion_disposition(status),
                DeletionDisposition::Dispatch("RequestDeletion"),
                "{status} must begin teardown-gated deletion"
            );
        }
        assert_eq!(
            deletion_disposition("DeletionFailed"),
            DeletionDisposition::Dispatch("RetryDeletion")
        );
    }

    #[test]
    fn deletion_retries_in_progress_and_authorizes_completed_idempotence() {
        assert_eq!(
            deletion_disposition("Deleting"),
            DeletionDisposition::Dispatch("RetryDeletion")
        );
        assert_eq!(
            deletion_disposition("Deleted"),
            DeletionDisposition::Deleted
        );
    }

    #[test]
    fn race_resolution_only_accepts_known_stale_dispatch_errors() {
        assert!(is_stale_deletion_dispatch_error(
            "Action 'RequestDeletion' not valid from state 'Deleting'"
        ));
        assert!(is_stale_deletion_dispatch_error(
            "action authorization became stale; retry against current state"
        ));
        assert!(!is_stale_deletion_dispatch_error(
            "authorization denied: no matching permit policy"
        ));
        assert!(!is_stale_deletion_dispatch_error("provider request failed"));
    }

    #[test]
    fn deletion_rejects_active_states() {
        for status in [
            "Created",
            "Provisioning",
            "Thinking",
            "Executing",
            "Compacting",
            "Steering",
            "Recovering",
        ] {
            assert_eq!(
                deletion_disposition(status),
                DeletionDisposition::Active,
                "{status}"
            );
        }
    }
}
