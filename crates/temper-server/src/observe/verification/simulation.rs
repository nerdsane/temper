//! GET /observe/simulation/{entity} -- run deterministic simulation.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use tracing::instrument;

use crate::authz::require_observe_auth;
use crate::observe::SimQueryParams;
use crate::state::ServerState;

/// GET /observe/simulation/{entity}?seed=N&ticks=M -- run deterministic simulation.
///
/// Runs a single-seed simulation with light fault injection and returns the result.
#[instrument(skip_all, fields(entity, otel.name = "GET /observe/simulation/{entity}"))]
pub(crate) async fn handle_run_simulation(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(entity): Path<String>,
    Query(params): Query<SimQueryParams>,
) -> Result<Json<temper_verify::SimulationResult>, StatusCode> {
    require_observe_auth(&state, &headers, "read_verification", "Verification")?;

    let Some((_tenant_id, ioa_source)) = state.find_entity_ioa_source(&entity) else {
        tracing::warn!("entity spec not found for simulation");
        return Err(StatusCode::NOT_FOUND);
    };

    let seed = params.seed.unwrap_or(42);
    let ticks = params.ticks.unwrap_or(200);

    let result = tokio::task::spawn_blocking(move || {
        let config = temper_verify::SimConfig {
            seed,
            max_ticks: ticks,
            num_actors: 3,
            max_actions_per_actor: 20,
            max_counter: 2,
            faults: temper_runtime::scheduler::FaultConfig::light(),
        };
        temper_verify::run_simulation_from_ioa(&ioa_source, &config)
    })
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "simulation task failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .map_err(|e| {
        tracing::error!(error = %e, "IOA parse failed in simulation");
        StatusCode::BAD_REQUEST
    })?;

    Ok(Json(result))
}
