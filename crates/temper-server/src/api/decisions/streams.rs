use std::convert::Infallible;

use axum::extract::{Extension, Path, State};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use temper_authz::AuthenticatedRequestContext;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::instrument;

use super::super::PolicyAuthed;
use crate::authz::{require_authenticated_context, require_observe_auth};
use crate::state::ServerState;

/// GET /api/tenants/{tenant}/decisions/stream — SSE for pending decisions.
#[instrument(skip_all, fields(tenant, otel.name = "GET /api/tenants/{tenant}/decisions/stream"))]
pub(crate) async fn handle_decision_stream(
    State(state): State<ServerState>,
    Path(_tenant): Path<String>,
    auth: PolicyAuthed,
) -> impl IntoResponse {
    let tenant = auth.tenant().as_str().to_string();
    let rx = state.pending_decision_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| match result {
        Ok(pd) if pd.tenant == tenant => {
            let data = serde_json::to_string(&pd).unwrap_or_default();
            Some(Ok::<Event, Infallible>(
                Event::default().event("pending_decision").data(data),
            ))
        }
        Ok(_) | Err(_) => None,
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// GET /api/decisions/stream — tenant-scoped pending decision stream.
#[instrument(skip_all, fields(otel.name = "GET /api/decisions/stream"))]
pub(crate) async fn handle_all_decisions_stream(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    if let Err(status) = require_observe_auth(&state, authenticated, "manage_policies", "PolicySet")
    {
        return (status, "Authorization required").into_response();
    }
    let tenant = authenticated.tenant().as_str().to_string();
    let rx = state.pending_decision_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| match result {
        Ok(pd) if pd.tenant == tenant => {
            let data = serde_json::to_string(&pd).unwrap_or_default();
            Some(Ok::<Event, Infallible>(
                Event::default().event("pending_decision").data(data),
            ))
        }
        Ok(_) | Err(_) => None,
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// GET /api/agents/{agent_id}/stream — SSE for agent progress events.
#[instrument(skip_all, fields(agent_id, otel.name = "GET /api/agents/{agent_id}/stream"))]
pub(crate) async fn handle_agent_progress_stream(
    State(state): State<ServerState>,
    Path(agent_id): Path<String>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    if let Err(status) = require_observe_auth(&state, authenticated, "read_agents", "AgentAudit") {
        return (status, "Authorization required").into_response();
    }
    let tenant = authenticated.tenant().as_str().to_string();
    let rx = state.agent_progress_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |result| match result {
        Ok(event) if event.tenant == tenant && event.agent_id == agent_id => {
            let data = serde_json::to_string(&event).unwrap_or_default();
            Some(Ok::<Event, Infallible>(
                Event::default().event(&event.kind).data(data),
            ))
        }
        Ok(_) | Err(_) => None,
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}
