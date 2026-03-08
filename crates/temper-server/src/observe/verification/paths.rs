//! GET /observe/paths/{entity} -- extract state machine paths.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::Deserialize;
use tracing::instrument;

use crate::authz::require_observe_auth;
use crate::state::ServerState;

/// Query parameters for path extraction.
#[derive(Deserialize)]
pub(crate) struct PathsQueryParams {
    pub targets: Option<String>,
    pub max_length: Option<usize>,
}

/// GET /observe/paths/{entity} -- extract reachable state paths.
#[instrument(skip_all, fields(entity, otel.name = "GET /observe/paths/{entity}"))]
pub(crate) async fn handle_get_paths(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(entity): Path<String>,
    Query(params): Query<PathsQueryParams>,
) -> Result<Json<temper_verify::PathExtractionResult>, StatusCode> {
    require_observe_auth(&state, &headers, "read_verification", "Verification")?;

    let Some((_tenant_id, ioa_source)) = state.find_entity_ioa_source(&entity) else {
        tracing::warn!("entity spec not found for path extraction");
        return Err(StatusCode::NOT_FOUND);
    };
    let target_states: Vec<String> = params
        .targets
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let max_length = params.max_length.unwrap_or(20);
    // determinism-ok: spawn_blocking for CPU-intensive path extraction in HTTP handler
    let result = tokio::task::spawn_blocking(move || {
        let model = temper_verify::build_model_from_ioa(&ioa_source, 2)
            .map_err(|e| format!("IOA parse error: {e}"))?;
        let config = temper_verify::PathExtractionConfig {
            target_states,
            max_path_length: max_length,
        };
        Ok::<_, String>(temper_verify::extract_paths(&model, &config))
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "path extraction task failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .map_err(|e| {
        tracing::error!(error = %e, "IOA parse failed in path extraction");
        StatusCode::BAD_REQUEST
    })?;
    Ok(Json(result))
}
