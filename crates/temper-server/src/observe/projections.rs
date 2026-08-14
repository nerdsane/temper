//! Projection correctness observe endpoints.

use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use temper_authz::AuthenticatedRequestContext;
use tracing::instrument;

use crate::authz::{require_authenticated_context, require_observe_auth};
use crate::state::ServerState;

const DEFAULT_REPLAY_PARITY_LIMIT: usize = 100;
const MAX_REPLAY_PARITY_LIMIT: usize = 1_000;

#[derive(Debug, Deserialize)]
pub(crate) struct ReplayParityParams {
    entity_type: Option<String>,
    limit: Option<usize>,
}

/// GET /observe/projections/replay-parity -- bounded event-replay parity probe.
#[instrument(
    skip_all,
    fields(
        otel.name = "GET /observe/projections/replay-parity",
        tenant = tracing::field::Empty,
        entity_type = tracing::field::Empty,
        limit = tracing::field::Empty,
        checked = tracing::field::Empty,
        drifted = tracing::field::Empty,
        missing = tracing::field::Empty,
        errors = tracing::field::Empty,
        clean = tracing::field::Empty
    )
)]
pub(crate) async fn handle_replay_parity(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Query(params): Query<ReplayParityParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_entities", "Projection")?;
    let tenant = authenticated.tenant();
    let entity_type = params
        .entity_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let limit = params
        .limit
        .unwrap_or(DEFAULT_REPLAY_PARITY_LIMIT)
        .clamp(1, MAX_REPLAY_PARITY_LIMIT);

    tracing::Span::current().record("tenant", tenant.as_str());
    tracing::Span::current().record("entity_type", entity_type.unwrap_or("*"));
    tracing::Span::current().record("limit", limit as u64);

    let report = state
        .verify_query_projection_replay_parity_bounded(
            tenant,
            entity_type,
            Some(limit),
            "observe_probe",
        )
        .await
        .map_err(|error| {
            tracing::warn!(tenant = %tenant, error = %error, "projection replay parity probe failed");
            StatusCode::SERVICE_UNAVAILABLE
        })?;

    tracing::Span::current().record("checked", report.checked);
    tracing::Span::current().record("drifted", report.drifted);
    tracing::Span::current().record("missing", report.missing);
    tracing::Span::current().record("errors", report.errors);
    tracing::Span::current().record("clean", report.is_clean());

    Ok(Json(serde_json::json!({
        "kind": "query_projection_replay_parity",
        "clean": report.is_clean(),
        "limit": limit,
        "report": report,
    })))
}
