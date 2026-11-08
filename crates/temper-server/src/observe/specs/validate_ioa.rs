use std::time::{Duration, Instant}; // determinism-ok: Instant measures the wait for the host's turn

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::Json;
use temper_authz::AuthenticatedRequestContext;
use tracing::instrument;

use crate::authz::{require_authenticated_context, require_observe_auth};
use crate::host_room::HOST_ROOM;
use crate::state::ServerState;

use super::kept;
use super::types::ValidateIoaRequest;

/// Simulation seeds one check runs when the caller names none.
pub const DEFAULT_SIM_SEEDS: u64 = 5;
/// Property-test cases one check runs when the caller names none.
pub const DEFAULT_PROP_TEST_CASES: u32 = 100;
const MAX_SIM_SEEDS: u64 = 100;
const MAX_PROP_TEST_CASES: u32 = 10_000;
/// Time one check may take when the caller names no budget. A WASM
/// integration waits at most 50 s on one call, so a check that runs out of
/// this still answers its caller, with why.
pub const DEFAULT_BUDGET_MS: u64 = 30_000;
/// The most time a caller may give one check.
pub const MAX_BUDGET_MS: u64 = 120_000;
/// The most states one check's model check may generate. A state reached
/// twice counts twice: a large app-run entity generates 5.3M (381k
/// distinct) in 14 s. A spec of cheap steps reaches this with about 1.4M
/// distinct states and 1.1 GB, the most one check holds.
const MAX_BUDGET_STATES: usize = 10_000_000;
/// What one generated state holds at most: the 1.1 GB above over its 10M
/// states. It turns the memory a check may use into the states it may walk.
const BYTES_PER_STATE: usize = 110;

/// POST /api/specs/validate-ioa -- validate IOA source without loading it.
///
/// This lets agents lint locally, then ask the running server/kernel to perform
/// the authoritative L0-L3 cascade before attempting a spec load.
///
/// The source is anyone's, and a few counters can make its check run for
/// hours on a thread nothing can cancel, so every check has a budget
/// (`budget_ms`, default 30 s). A check that runs out answers 200 with the
/// level it was in failed, saying so, and `stopped` naming the type.
///
/// A passing verdict is kept by the digest of what the levels read, and a
/// spec with the same digest gets it back at once with `kept` set: a change
/// to text no level reads, such as an error message, is not checked again.
///
/// The check runs in this host's turn ([`crate::host_turn`]), and the span's
/// `waited_ms` says how long it waited for it.
#[instrument(
    skip_all,
    fields(
        otel.name = "POST /api/specs/validate-ioa",
        kept = tracing::field::Empty,
        waited_ms = tracing::field::Empty,
    )
)]
pub(crate) async fn handle_validate_ioa(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Json(body): Json<ValidateIoaRequest>,
) -> Result<Json<temper_verify::CascadeResult>, (StatusCode, String)> {
    let authenticated = require_authenticated_context(authenticated.as_deref())
        .map_err(|status| (status, "authentication required".to_string()))?;
    require_observe_auth(&state, authenticated, "run_verification", "Verification")
        .map_err(|status| (status, "verification authorization failed".to_string()))?;

    let ioa_source = body.ioa_source;
    if ioa_source.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "ioa_source must not be empty".to_string(),
        ));
    }
    // No limit on the source's size: what a check costs is its time and the states it walks, which the budget below
    // bounds (`MAX_BUDGET_MS`, `MAX_BUDGET_STATES`), not how long its text is.

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

    let budget_ms = body.budget_ms.unwrap_or(DEFAULT_BUDGET_MS);
    if !(1..=MAX_BUDGET_MS).contains(&budget_ms) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("budget_ms must be between 1 and {MAX_BUDGET_MS}"),
        ));
    }
    let asked = Instant::now(); // determinism-ok: how long this request waited for the host's turn
    let _turn = crate::host_turn::take()
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    tracing::Span::current().record("waited_ms", asked.elapsed().as_millis() as u64);
    let result = check_ioa(
        &state,
        ioa_source,
        sim_seeds,
        prop_test_cases,
        Duration::from_millis(budget_ms),
    )
    .await
    .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;

    tracing::Span::current().record("kept", result.kept.is_some());
    if let Some(stopped) = &result.stopped {
        tracing::warn!(budget_ms, stopped = %stopped, "validate-ioa ran out of its budget");
    }
    Ok(Json(result))
}

/// Check one spec's IOA source through the L0-L3 cascade within `time`,
/// unless a passing verdict is kept for what its levels read.
///
/// The model check may generate as many states as this host has memory for
/// ([`budget_states`]). A check that runs out of either answers with the
/// level it was in failed and `stopped` naming why. `Err` only when the
/// check's worker failed.
///
/// The caller holds this host's turn ([`crate::host_turn::take`]) for as long
/// as its work runs; this takes none, so a caller that also installs holds
/// one turn for both.
pub async fn check_ioa(
    state: &ServerState,
    ioa_source: String,
    sim_seeds: u64,
    prop_test_cases: u32,
    time: Duration,
) -> Result<temper_verify::CascadeResult, String> {
    let budget = temper_verify::Budget {
        time,
        states: budget_states(),
    };
    let check = move || {
        temper_verify::VerificationCascade::from_ioa(&ioa_source)
            .with_sim_seeds(sim_seeds)
            .with_prop_test_cases(prop_test_cases)
            .with_budget(budget)
    };
    let key = kept::key(check.clone()).await;
    if let Some(key) = &key
        && let Some(kept_result) = kept::kept(state, key).await
    {
        return Ok(kept_result);
    }
    // determinism-ok: an HTTP handler offloads CPU-bound verification
    let result = tokio::task::spawn_blocking(move || check().run())
        .await
        .map_err(|error| format!("verification worker failed: {error}"))?;
    if let Some(key) = &key {
        kept::keep(state, &key.digest, &result).await;
    }
    Ok(result)
}

/// The states one check may generate now: [`MAX_BUDGET_STATES`], or fewer
/// when this host leaves joint walks less memory than that takes
/// ([`HOST_ROOM`]), since a check's states live in the same memory. A host
/// whose free memory cannot be read keeps the fixed bound it had before.
/// One sandbox runs several app servers under one memory limit, and a
/// check sized for 1.1 GB on a 2 GB sandbox kills every server on it.
pub fn budget_states() -> usize {
    match HOST_ROOM.left() {
        Some(room) => MAX_BUDGET_STATES.min(room / BYTES_PER_STATE),
        None => MAX_BUDGET_STATES,
    }
}
