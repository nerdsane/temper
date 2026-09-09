use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::scheduler::{sim_now, sim_uuid};
use tracing::instrument;

use crate::authz::{
    observe_tenant_scope, require_authenticated_context, require_observe_auth, require_tenant_match,
};
use crate::ots_trajectory_outbox::{OtsTrajectoryEnqueueError, OtsTrajectoryWrite};
use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};

/// Query parameters for the trajectory aggregation endpoint.
#[derive(Deserialize)]
pub(crate) struct TrajectoryQueryParams {
    /// Filter by entity type.
    pub entity_type: Option<String>,
    /// Filter by action name.
    pub action: Option<String>,
    /// Filter by success/failure ("true" or "false").
    pub success: Option<String>,
    /// Maximum number of failed intents to return in the response (default: 50).
    pub failed_limit: Option<usize>,
}

/// GET /observe/trajectories -- aggregated trajectory stats.
///
/// Returns:
/// - `total`: total matching entries
/// - `success_count` / `error_count` / `success_rate`
/// - `by_action`: per-action breakdown
/// - `failed_intents`: most recent failed entries (up to `failed_limit`)
#[instrument(skip_all, fields(otel.name = "GET /observe/trajectories"))]
pub(crate) async fn handle_trajectories(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Query(params): Query<TrajectoryQueryParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_trajectories", "Trajectory")?;
    let tenant_scope = observe_tenant_scope(authenticated);
    let failed_limit = params.failed_limit.unwrap_or(50).min(500);
    let success_filter: Option<bool> = params.success.as_deref().map(|s| s == "true");

    let store = state.metadata_store_for_tenant(tenant_scope.as_str()).await;
    let stores = store.into_iter().collect::<Vec<_>>();

    if !stores.is_empty() {
        // Aggregate stats across all queried stores.
        let mut total: u64 = 0;
        let mut success_count: u64 = 0;
        let mut error_count: u64 = 0;
        let mut by_action: std::collections::BTreeMap<String, temper_store_turso::ActionStats> =
            std::collections::BTreeMap::new();
        let mut failed_intents = Vec::new();

        for store in &stores {
            match store
                .query_trajectory_stats(
                    tenant_scope.as_str(),
                    params.entity_type.as_deref(),
                    params.action.as_deref(),
                    success_filter,
                    failed_limit as i64,
                )
                .await
            {
                Ok(stats) => {
                    total += stats.total;
                    success_count += stats.success_count;
                    error_count += stats.error_count;
                    for (action, action_stats) in stats.by_action {
                        let entry =
                            by_action
                                .entry(action)
                                .or_insert(temper_store_turso::ActionStats {
                                    total: 0,
                                    success: 0,
                                    error: 0,
                                });
                        entry.total += action_stats.total;
                        entry.success += action_stats.success;
                        entry.error += action_stats.error;
                    }
                    failed_intents.extend(stats.failed_intents);
                }
                Err(e) => {
                    tracing::warn!(error = %e, backend = store.backend_name(), "failed to query trajectories");
                }
            }
        }

        let success_rate = if total > 0 {
            success_count as f64 / total as f64
        } else {
            0.0
        };
        // Sort and limit failed intents
        failed_intents.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        failed_intents.truncate(failed_limit);

        return Ok(Json(serde_json::json!({
            "total": total,
            "success_count": success_count,
            "error_count": error_count,
            "success_rate": success_rate,
            "by_action": by_action,
            "failed_intents": failed_intents,
        })));
    }

    // Fallback: empty response when no durable metadata backend is configured.
    Ok(Json(serde_json::json!({
        "total": 0,
        "success_count": 0,
        "error_count": 0,
        "success_rate": 0.0,
        "by_action": {},
        "failed_intents": [],
    })))
}

/// POST /api/evolution/trajectories/unmet -- record an unmet user intent.
///
/// Called by the production chat proxy when a user asks for something
/// that doesn't map to any available action. This feeds the Evolution Engine.
///
/// The row's tenant, agent id, and agent type come from the credential; the
/// descriptive fields — entity type, action name, session — come from the request
/// body, so the row is a caller's account of something that never reached a
/// governed dispatch. It is written `spec_governed = false` for
/// that reason: the conformance checker judges governed dispatches, and a row
/// any caller can post under any session and entity type would otherwise let
/// one caller inject violations into another run's report
/// (`crate::conformance::walk::row_disposition`).
#[instrument(skip_all, fields(otel.name = "POST /api/evolution/trajectories/unmet"))]
pub(crate) async fn handle_unmet_intent(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Json(body): Json<serde_json::Value>,
) -> Result<StatusCode, (StatusCode, String)> {
    let authenticated = require_authenticated_context(authenticated.as_deref())
        .map_err(|status| (status, "unauthorized".to_string()))?;
    require_observe_auth(&state, authenticated, "write_trajectories", "Trajectory")
        .map_err(|sc| (sc, "unauthorized".to_string()))?;

    if !state.enqueue_trajectory_entry(unmet_intent_entry(authenticated, &body)?) {
        tracing::warn!("failed to enqueue unmet-intent trajectory");
    }

    Ok(StatusCode::CREATED)
}

/// Build the row an unmet-intent report becomes.
///
/// The row's tenant comes from the credential, never from the body: authorizing
/// against one tenant and writing into another is the split-brain ADR-0157 closes.
fn unmet_intent_entry(
    authenticated: &AuthenticatedRequestContext,
    body: &serde_json::Value,
) -> Result<TrajectoryEntry, (StatusCode, String)> {
    let intent = body
        .get("action")
        .or_else(|| body.get("intent"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    if let Some(requested_tenant) = body.get("tenant").and_then(|value| value.as_str()) {
        require_tenant_match(authenticated, requested_tenant)
            .map_err(|status| (status, "tenant mismatch".to_string()))?;
    }
    let tenant = authenticated.tenant().as_str();
    let entity_type = body
        .get("entity_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let error_msg = body.get("error").and_then(|v| v.as_str()).unwrap_or("");

    Ok(TrajectoryEntry {
        timestamp: sim_now().to_rfc3339(),
        tenant: tenant.to_string(),
        entity_type: entity_type.to_string(),
        entity_id: "".to_string(),
        action: intent.to_string(),
        success: false,
        from_status: None,
        to_status: None,
        agent_id: Some(authenticated.security_context().principal.id.clone()),
        session_id: body
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: body
            .get("source")
            .and_then(|v| v.as_str())
            .and_then(|s| match s {
                "platform" => Some(TrajectorySource::Platform),
                "authz" => Some(TrajectorySource::Authz),
                "entity" => Some(TrajectorySource::Entity),
                _ => None,
            }),
        error: Some(if error_msg.is_empty() {
            format!("Unmet intent: {intent}")
        } else {
            error_msg.to_string()
        }),
        // An unmet intent is by definition an action the kernel never
        // dispatched, and the row body is caller-supplied. Both make it a
        // report about the run rather than a record of it, so it stays out of
        // conformance verdicts.
        spec_governed: Some(false),
        // Provenance comes from the authenticated credential, not the body.
        agent_type: authenticated
            .security_context()
            .principal
            .agent_type
            .clone(),
        request_body: body.get("request_body").cloned(),
        intent: body
            .get("intent")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| Some(intent.to_string())),
        matched_policy_ids: None,
        capture_seq: None,
    })
}

// ---------------------------------------------------------------------------
// OTS Trajectory endpoints — full agent execution traces for GEPA
// ---------------------------------------------------------------------------

/// Query parameters for OTS trajectory listing.
#[derive(Deserialize)]
pub(crate) struct OtsTrajectoryQueryParams {
    pub agent_id: Option<String>,
    pub outcome: Option<String>,
    pub limit: Option<i64>,
}

/// POST /api/ots/trajectories — receive a full OTS trajectory from an MCP session.
///
/// The body is parsed as an [`OTSTrajectory`], not as free JSON. Two things
/// depend on that:
///
/// - **Identity.** The run's id is the document's top-level `trajectory_id`.
///   It is what the uploader holds and what
///   `GET /api/ots/trajectories/{id}/atif` and a conformance check address the
///   row by, so storing anything else makes a successfully uploaded run
///   unreachable.
/// - **The token-signal contract.** `OTSTurn` refuses to deserialize a turn
///   whose completion-side signals disagree, so a misaligned training sample
///   is rejected at the door instead of persisted and later exported as valid
///   RL data.
#[instrument(skip_all, fields(otel.name = "POST /api/ots/trajectories"))]
pub(crate) async fn handle_post_ots_trajectory(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    body: String,
) -> Result<StatusCode, (StatusCode, String)> {
    let authenticated = require_authenticated_context(authenticated.as_deref())
        .map_err(|status| (status, "unauthorized".to_string()))?;
    require_observe_auth(&state, authenticated, "write_trajectories", "OtsTrajectory")
        .map_err(|status| (status, "unauthorized".to_string()))?;
    let trajectory: temper_ots::models::OTSTrajectory =
        serde_json::from_str(&body).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("not a valid OTS trajectory: {e}"),
            )
        })?;

    let trajectory_id = if trajectory.trajectory_id.is_empty() {
        // Uploads have carried an id since the format existed; generating one
        // keeps a legacy producer working, at the cost of an id it cannot
        // address the row by.
        let generated = sim_uuid().to_string();
        tracing::warn!(
            generated_trajectory_id = %generated,
            "OTS upload carried no trajectory_id; storing under a generated id"
        );
        generated
    } else {
        trajectory.trajectory_id.clone()
    };

    let agent_id = authenticated.security_context().principal.id.as_str();

    // The uploader's declared session, carried on the request context by the
    // bearer edge (never a raw header read, and never a Cedar input).
    let session_id = authenticated.session_id().unwrap_or("");

    let outcome = match trajectory.metadata.outcome {
        temper_ots::models::OutcomeType::Success => "success",
        temper_ots::models::OutcomeType::PartialSuccess => "partial_success",
        temper_ots::models::OutcomeType::Failure => "failure",
    };

    let turn_count = trajectory.turns.len() as i64;

    let tenant = authenticated.tenant().as_str();

    let Some(store) = state.metadata_store_for_tenant(tenant).await else {
        tracing::warn!(
            tenant = %tenant,
            "no persistent store — OTS trajectory not persisted"
        );
        return Ok(StatusCode::CREATED);
    };

    let Some((backend, outbox)) = state.ots_trajectory_outbox() else {
        tracing::error!(
            tenant = %tenant,
            trajectory_id = %trajectory_id,
            "OTS trajectory outbox unavailable despite configured metadata store"
        );
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "OTS trajectory persistence queue unavailable; retry upload".to_string(),
        ));
    };

    let write = OtsTrajectoryWrite {
        trajectory_id,
        tenant: tenant.to_string(),
        agent_id: agent_id.to_string(),
        session_id: session_id.to_string(),
        outcome: outcome.to_string(),
        turn_count,
        data: body,
    };

    match outbox
        .try_enqueue_metadata_store(backend, store, write)
        .await
    {
        Ok(()) => {
            tracing::info!(
                tenant = %tenant,
                agent_id = %agent_id,
                session_id = %session_id,
                turn_count = turn_count,
                outcome = %outcome,
                "ots.trajectory.queued"
            );
            Ok(StatusCode::ACCEPTED)
        }
        Err(OtsTrajectoryEnqueueError::Full) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "OTS trajectory persistence queue full; retry upload".to_string(),
        )),
        Err(OtsTrajectoryEnqueueError::Closed) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "OTS trajectory persistence queue closed; retry upload".to_string(),
        )),
    }
}

/// GET /api/ots/trajectories — list OTS trajectories with optional filters.
#[instrument(skip_all, fields(otel.name = "GET /api/ots/trajectories"))]
pub(crate) async fn handle_get_ots_trajectories(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Query(params): Query<OtsTrajectoryQueryParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let authenticated = require_authenticated_context(authenticated.as_deref())
        .map_err(|status| (status, "unauthorized".to_string()))?;
    require_observe_auth(&state, authenticated, "read_trajectories", "OtsTrajectory")
        .map_err(|status| (status, "unauthorized".to_string()))?;
    let tenant = authenticated.tenant().as_str();
    let limit = params.limit.unwrap_or(50);
    if !(1..=500).contains(&limit) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("limit must be between 1 and 500, got {limit}"),
        ));
    }

    let Some(store) = state.metadata_store_for_tenant(tenant).await else {
        return Ok(Json(serde_json::json!({
            "trajectories": [],
            "total": 0,
        })));
    };

    match store
        .list_ots_trajectories(
            tenant,
            params.agent_id.as_deref(),
            params.outcome.as_deref(),
            limit,
        )
        .await
    {
        Ok(rows) => {
            let total = rows.len();
            Ok(Json(serde_json::json!({
                "trajectories": rows,
                "total": total,
            })))
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to list OTS trajectories");
            Ok(Json(serde_json::json!({
                "trajectories": [],
                "total": 0,
            })))
        }
    }
}

#[cfg(test)]
#[path = "trajectories_route_test.rs"]
mod route_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{ConformanceInput, SpecResolution, check_conformance};
    use temper_spec::automaton::parse_automaton;

    const ORDER_IOA: &str = include_str!("../../../../../test-fixtures/specs/order.ioa.toml");

    #[test]
    fn an_unmet_intent_row_is_never_a_governed_dispatch() {
        // Even when the caller declares an entity source, a governed session,
        // and a real actor's entity type.
        let authenticated = AuthenticatedRequestContext::new(
            temper_runtime::tenant::TenantId::default(),
            temper_authz::SecurityContext::system(),
        );
        let entry = unmet_intent_entry(
            &authenticated,
            &serde_json::json!({
                "tenant": "default",
                "entity_type": "Order",
                "action": "ShipOrder",
                "session_id": "session-1",
                "source": "entity",
            }),
        )
        .expect("the system context is bound to the default tenant");

        assert_eq!(
            entry.spec_governed,
            Some(false),
            "the kernel never dispatched this action; the row is a report about the run"
        );
    }

    #[tokio::test]
    async fn an_unmet_intent_row_cannot_inject_a_violation_into_a_session() {
        // The whole row is caller-chosen, so without the exclusion any caller
        // could post an illegal transition into another run's report. Written
        // and read back through a real store, because what the checker sees is
        // the stored row, not the entry.
        let dir = tempfile::tempdir().expect("create temp dir");
        let db_url = format!("file:{}", dir.path().join("unmet.db").display());
        let store = temper_store_turso::TursoEventStore::new(&db_url, None)
            .await
            .expect("create local turso store");

        let authenticated = AuthenticatedRequestContext::new(
            temper_runtime::tenant::TenantId::default(),
            temper_authz::SecurityContext::system(),
        );
        let entry = unmet_intent_entry(
            &authenticated,
            &serde_json::json!({
                "tenant": "default",
                "entity_type": "Order",
                "action": "ShipOrder",
                "session_id": "session-1",
                "source": "entity",
            }),
        )
        .expect("the system context is bound to the default tenant");
        crate::storage::TrajectorySink::persist_trajectory_entry(&store, &entry)
            .await
            .expect("persist unmet intent");

        let rows = store
            .query_trajectories_by_session("session-1", Some("default"), None, 10)
            .await
            .expect("read the session back");
        assert_eq!(rows.len(), 1, "the row is stored and readable");

        let automaton = parse_automaton(ORDER_IOA).expect("order fixture parses");
        let report = check_conformance(ConformanceInput {
            automaton: &automaton,
            kernel_rows: &rows,
            ots_trajectory: None,
            rows_truncated: false,
            spec_resolution: SpecResolution::Pinned,
            capture_degraded: false,
        });

        assert!(
            report.violations.is_empty(),
            "a caller-supplied unmet intent is not this actor executing its spec: {:?}",
            report.violations
        );
        assert_eq!(report.stats.non_governed_rows_skipped, 1);
        assert_eq!(report.stats.actor_rows, 0);
    }
}
