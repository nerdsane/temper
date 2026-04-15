use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::Deserialize;
use temper_runtime::scheduler::{sim_now, sim_uuid};
use tracing::instrument;

use crate::authz::{observe_tenant_scope, require_observe_auth};
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

/// GET /observe/trajectories -- aggregated trajectory stats from Turso.
///
/// Returns:
/// - `total`: total matching entries
/// - `success_count` / `error_count` / `success_rate`
/// - `by_action`: per-action breakdown
/// - `failed_intents`: most recent failed entries (up to `failed_limit`)
#[instrument(skip_all, fields(otel.name = "GET /observe/trajectories"))]
pub(crate) async fn handle_trajectories(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(params): Query<TrajectoryQueryParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "read_trajectories", "Trajectory")?;
    let tenant_scope = observe_tenant_scope(&state, &headers)?;
    let failed_limit = params.failed_limit.unwrap_or(50).min(500);
    let success_filter: Option<bool> = params.success.as_deref().map(|s| s == "true");

    // Determine which stores to query: tenant-scoped or fan-out.
    let stores = if let Some(ref scope) = tenant_scope {
        match state.persistent_store_for_tenant(scope.as_str()).await {
            Some(turso) => vec![turso],
            None => Vec::new(),
        }
    } else {
        state.collect_all_turso_stores().await
    };

    if !stores.is_empty() {
        // Aggregate stats across all queried stores.
        let mut total: u64 = 0;
        let mut success_count: u64 = 0;
        let mut error_count: u64 = 0;
        let mut by_action: std::collections::BTreeMap<String, temper_store_turso::ActionStats> =
            std::collections::BTreeMap::new();
        let mut failed_intents = Vec::new();

        for turso in &stores {
            match turso
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
                    tracing::warn!(error = %e, "failed to query trajectories from Turso");
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

    // Fallback: empty response when no Turso configured.
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
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<StatusCode, (StatusCode, String)> {
    require_observe_auth(&state, &headers, "write_trajectories", "Trajectory")
        .map_err(|sc| (sc, "unauthorized".to_string()))?;

    let intent = body
        .get("action")
        .or_else(|| body.get("intent"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let tenant = body
        .get("tenant")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
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
        agent_id: body
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
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
        agent_type: None,
        request_body: body.get("request_body").cloned(),
        intent: body
            .get("intent")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| Some(intent.to_string())),
        matched_policy_ids: None,
    };
    state
        .persist_trajectory_entry(&entry)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

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
    body: String,
) -> Result<StatusCode, (StatusCode, String)> {
    // Parse the OTS trajectory JSON to extract indexed fields.
    let trajectory: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")))?;

    let trajectory_id = trajectory
        .get("metadata")
        .and_then(|m| m.get("trajectory_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| sim_uuid().to_string());

    let agent_id = headers
        .get("X-Agent-Id")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            trajectory
                .get("metadata")
                .and_then(|m| m.get("agent_id"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("unknown");

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

    let tenant = headers
        .get("X-Tenant-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("default");

    if let Some(turso) = state.persistent_store_for_tenant(tenant).await {
        turso
            .persist_ots_trajectory(&temper_store_turso::OtsTrajectoryParams {
                trajectory_id: &trajectory_id,
                tenant,
                agent_id,
                session_id,
                outcome,
                turn_count,
                data: &body,
            })
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to persist OTS trajectory: {e}"),
                )
            })?;

        tracing::info!(
            trajectory_id = %trajectory_id,
            agent_id = %agent_id,
            turn_count = turn_count,
            outcome = %outcome,
            "ots.trajectory.persisted"
        );
    } else {
        tracing::warn!(
            tenant = %tenant,
            "no persistent store — OTS trajectory not persisted"
        );
    }

    Ok(StatusCode::CREATED)
}

/// GET /api/ots/trajectories — list OTS trajectories with optional filters.
#[instrument(skip_all, fields(otel.name = "GET /api/ots/trajectories"))]
pub(crate) async fn handle_get_ots_trajectories(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(params): Query<OtsTrajectoryQueryParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let tenant = headers
        .get("X-Tenant-Id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("default");
    let limit = params.limit.unwrap_or(50).min(500);

    let Some(turso) = state.persistent_store_for_tenant(tenant).await else {
        return Ok(Json(serde_json::json!({
            "trajectories": [],
            "total": 0,
        })));
    };

    match turso
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
