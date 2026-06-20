use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use tracing::instrument;

use crate::authz::require_observe_auth;
use crate::state::ServerState;

use super::types::ValidateIoaRequest;

const DEFAULT_SIM_SEEDS: u64 = 5;
const DEFAULT_PROP_TEST_CASES: u32 = 100;
const MAX_SIM_SEEDS: u64 = 100;
const MAX_PROP_TEST_CASES: u32 = 10_000;
const MAX_IOA_SOURCE_BYTES: usize = 1_048_576;

/// POST /api/specs/validate-ioa -- validate IOA source without loading it.
///
/// This lets agents lint locally, then ask the running server/kernel to perform
/// the authoritative L0-L3 cascade before attempting a spec load.
#[instrument(skip_all, fields(otel.name = "POST /api/specs/validate-ioa"))]
pub(crate) async fn handle_validate_ioa(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<ValidateIoaRequest>,
) -> Result<Json<temper_verify::CascadeResult>, (StatusCode, String)> {
    require_observe_auth(&state, &headers, "run_verification", "Verification")
        .map_err(|status| (status, "verification authorization failed".to_string()))?;

    let ioa_source = body.ioa_source;
    if ioa_source.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "ioa_source must not be empty".to_string(),
        ));
    }
    if ioa_source.len() > MAX_IOA_SOURCE_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("ioa_source exceeds {MAX_IOA_SOURCE_BYTES} bytes"),
        ));
    }

    let sim_seeds = body.sim_seeds.unwrap_or(DEFAULT_SIM_SEEDS);
    if sim_seeds > MAX_SIM_SEEDS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("sim_seeds must be <= {MAX_SIM_SEEDS}"),
        ));
    }

    let prop_test_cases = body.prop_test_cases.unwrap_or(DEFAULT_PROP_TEST_CASES);
    if prop_test_cases > MAX_PROP_TEST_CASES {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("prop_test_cases must be <= {MAX_PROP_TEST_CASES}"),
        ));
    }

    let result = tokio::task::spawn_blocking(move || {
        temper_verify::VerificationCascade::from_ioa(&ioa_source)
            .with_sim_seeds(sim_seeds)
            .with_prop_test_cases(prop_test_cases)
            .run()
    })
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("verification worker failed: {error}"),
        )
    })?;

    Ok(Json(result))
}
