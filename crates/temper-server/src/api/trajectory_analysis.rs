//! Trajectory analysis endpoints: conformance checking and ATIF export.
//!
//! Both read recorded trajectories, so both carry the same authorization as
//! the other trajectory reads under `/observe`: `read_trajectories` on
//! `Trajectory`, with the tenant resolved through `observe_tenant_scope`.
//!
//! Unlike the aggregate `/observe/trajectories` view, these two endpoints
//! address one tenant's data by identifier — a session id, a trajectory id —
//! so a cross-tenant admin view has nothing to resolve against. Both require
//! an explicit `X-Tenant-Id` and answer `400` without one.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::Deserialize;
use temper_ots::AtifTrajectory;
use temper_ots::models::OTSTrajectory;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::Automaton;
use tracing::instrument;

use crate::authz::{observe_tenant_scope, require_observe_auth};
use crate::conformance::check_conformance;
use crate::state::ServerState;

/// Largest session the conformance checker will read in one request.
///
/// The checker holds the whole session in memory and the state-machine walk is
/// order-dependent, so it cannot be paged; the cap bounds the read instead.
const MAX_CONFORMANCE_ROWS: i64 = 5_000;

/// Body of `POST /api/conformance/check`.
#[derive(Debug, Deserialize)]
pub(crate) struct ConformanceCheckRequest {
    /// The actor spec to check the run against.
    entity_type: String,
    /// The session whose rows form the run.
    session_id: String,
    /// Optional OTS trajectory contributing the agent-side decisions.
    #[serde(default)]
    trajectory_id: Option<String>,
    /// Cap on rows read, bounded by [`MAX_CONFORMANCE_ROWS`].
    #[serde(default)]
    limit: Option<i64>,
}

/// POST /api/conformance/check — check one session against its actor spec.
#[instrument(skip_all, fields(otel.name = "POST /api/conformance/check"))]
pub(crate) async fn handle_conformance_check(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(request): Json<ConformanceCheckRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    require_observe_auth(&state, &headers, "read_trajectories", "Trajectory")
        .map_err(|status| (status, "unauthorized".to_string()))?;
    let tenant = required_tenant(&state, &headers)?;

    let limit = match request.limit {
        Some(limit) if !(1..=MAX_CONFORMANCE_ROWS).contains(&limit) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("limit must be between 1 and {MAX_CONFORMANCE_ROWS}"),
            ));
        }
        Some(limit) => limit,
        None => MAX_CONFORMANCE_ROWS,
    };

    let automaton = automaton_for(&state, &tenant, &request.entity_type)?;

    let Some(store) = state.metadata_store_for_tenant(tenant.as_str()).await else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no durable metadata backend configured; conformance needs stored trajectories"
                .to_string(),
        ));
    };

    // Entity type is deliberately not pushed into the query: the checker
    // reports rows from other actors as skipped, which is how a caller learns
    // a session touched entities this spec does not govern.
    let rows = store
        .query_trajectories_by_session(&request.session_id, Some(tenant.as_str()), None, limit)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "failed to read session trajectories");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read session trajectories: {error}"),
            )
        })?;

    let ots = match request.trajectory_id.as_deref() {
        Some(trajectory_id) => {
            let (ots_session_id, trajectory) =
                load_ots_trajectory(&store, &tenant, trajectory_id).await?;
            // Folding another run's decisions into this session's report would
            // produce violations that belong to neither run.
            if !ots_session_id.is_empty() && ots_session_id != request.session_id {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "trajectory `{trajectory_id}` belongs to session `{ots_session_id}`, \
                         not `{}`",
                        request.session_id
                    ),
                ));
            }
            Some(trajectory)
        }
        None => None,
    };

    let report = check_conformance(&automaton, &rows, ots.as_ref());
    // The walk is order-dependent and cannot be paged, so a session larger
    // than the cap is checked only up to the cap. Say so rather than let the
    // caller read a partial report as a whole one.
    let truncated = rows.len() as i64 == limit;
    tracing::info!(
        tenant = %tenant,
        entity_type = %request.entity_type,
        session_id = %request.session_id,
        rows = rows.len(),
        truncated,
        passed = report.passed,
        violations = report.violations.len(),
        "conformance.check"
    );

    Ok(Json(serde_json::json!({
        "tenant": tenant.as_str(),
        "entity_type": request.entity_type,
        "session_id": request.session_id,
        "trajectory_id": request.trajectory_id,
        "row_limit": limit,
        "truncated": truncated,
        "report": report,
    })))
}

/// GET /api/ots/trajectories/{id}/atif — export one trajectory as ATIF v1.7.
#[instrument(skip_all, fields(otel.name = "GET /api/ots/trajectories/{id}/atif"))]
pub(crate) async fn handle_get_ots_trajectory_atif(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(trajectory_id): Path<String>,
) -> Result<Json<AtifTrajectory>, (StatusCode, String)> {
    require_observe_auth(&state, &headers, "read_trajectories", "Trajectory")
        .map_err(|status| (status, "unauthorized".to_string()))?;
    let tenant = required_tenant(&state, &headers)?;

    let Some(store) = state.metadata_store_for_tenant(tenant.as_str()).await else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no durable metadata backend configured; nothing to export".to_string(),
        ));
    };

    let (session_id, trajectory) = load_ots_trajectory(&store, &tenant, &trajectory_id).await?;
    // The session lives on the storage row, not in the OTS document; an empty
    // column means the upload carried no session header, so ATIF omits it
    // rather than reporting an empty run identity.
    let session_id = (!session_id.is_empty()).then_some(session_id);
    Ok(Json(temper_ots::to_atif(
        &trajectory,
        session_id.as_deref(),
    )))
}

/// Resolve the tenant, requiring an explicit one.
fn required_tenant(
    state: &ServerState,
    headers: &HeaderMap,
) -> Result<TenantId, (StatusCode, String)> {
    let scope = observe_tenant_scope(state, headers)
        .map_err(|status| (status, "unauthorized".to_string()))?;
    scope.ok_or((
        StatusCode::BAD_REQUEST,
        "X-Tenant-Id is required: this endpoint addresses one tenant's trajectories".to_string(),
    ))
}

/// Load the actor spec for `entity_type`, cloned out of the registry lock.
fn automaton_for(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
) -> Result<Automaton, (StatusCode, String)> {
    let registry = state.registry.read().map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "spec registry lock poisoned".to_string(),
        )
    })?;
    registry
        .get_spec(tenant, entity_type)
        .map(|spec| spec.automaton.clone())
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("no spec registered for entity type `{entity_type}` in tenant `{tenant}`"),
        ))
}

/// Load and parse a stored OTS trajectory, returning its session id with it.
///
/// A missing row is a `404`; stored data that no longer parses is a `500` and
/// is logged, because that is a storage fault rather than a bad request.
async fn load_ots_trajectory(
    store: &std::sync::Arc<dyn crate::storage::MetadataStore>,
    tenant: &TenantId,
    trajectory_id: &str,
) -> Result<(String, OTSTrajectory), (StatusCode, String)> {
    let document = store
        .get_ots_trajectory(tenant.as_str(), trajectory_id)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "failed to read OTS trajectory");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read OTS trajectory: {error}"),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            format!("no OTS trajectory `{trajectory_id}` in tenant `{tenant}`"),
        ))?;

    let trajectory: OTSTrajectory = serde_json::from_str(&document.data).map_err(|error| {
        tracing::error!(
            error = %error,
            trajectory_id = %trajectory_id,
            "stored OTS trajectory does not parse"
        );
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("stored OTS trajectory `{trajectory_id}` does not parse: {error}"),
        )
    })?;
    Ok((document.session_id, trajectory))
}

#[cfg(test)]
#[path = "trajectory_analysis_test.rs"]
mod trajectory_analysis_test;
