use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, StatusCode};
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

    let entry = TrajectoryEntry {
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
        spec_governed: None,
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
    };
    if !state.enqueue_trajectory_entry(entry) {
        tracing::warn!("failed to enqueue unmet-intent trajectory");
    }

    Ok(StatusCode::CREATED)
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
#[instrument(skip_all, fields(otel.name = "POST /api/ots/trajectories"))]
pub(crate) async fn handle_post_ots_trajectory(
    State(state): State<ServerState>,
    headers: HeaderMap,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    body: String,
) -> Result<StatusCode, (StatusCode, String)> {
    let authenticated = require_authenticated_context(authenticated.as_deref())
        .map_err(|status| (status, "unauthorized".to_string()))?;
    require_observe_auth(&state, authenticated, "write_trajectories", "OtsTrajectory")
        .map_err(|status| (status, "unauthorized".to_string()))?;
    // Parse the OTS trajectory JSON to extract indexed fields.
    let trajectory: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")))?;

    let trajectory_id = trajectory
        .get("metadata")
        .and_then(|m| m.get("trajectory_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| sim_uuid().to_string());

    let agent_id = authenticated.security_context().principal.id.as_str();

    let session_id = headers
        .get("X-Session-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let outcome = trajectory
        .get("metadata")
        .and_then(|m| m.get("outcome"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let turn_count = trajectory
        .get("turns")
        .and_then(|t| t.as_array())
        .map(|a| a.len() as i64)
        .unwrap_or(0);

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
) -> Result<Json<serde_json::Value>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_trajectories", "OtsTrajectory")?;
    let tenant = authenticated.tenant().as_str();
    let limit = params.limit.unwrap_or(50).min(500);

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
