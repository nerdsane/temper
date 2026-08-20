//! Agent-run status handler.

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use opentelemetry::trace::TraceContextExt;
use temper_authz::AuthenticatedRequestContext;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::agent_runtime::models::RunStatus;
use crate::state::ServerState;

use super::common::{AGENT_ENTITY_TYPE, error_response, require_agent_app_contract, require_auth};

/// Get the status of an agent run.
#[tracing::instrument(
    skip_all,
    fields(
        otel.name = "agent.run.get",
        agent.run_id = %id,
    )
)]
pub(super) async fn get_run(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(id): Path<String>,
) -> Response {
    let (tenant, _authenticated) = match require_auth(authenticated) {
        Ok(t) => t,
        Err(r) => return *r,
    };
    if let Err(response) = require_agent_app_contract(&state, &tenant) {
        return *response;
    }

    let entity_state = match state
        .get_tenant_entity_state(&tenant, AGENT_ENTITY_TYPE, &id)
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
