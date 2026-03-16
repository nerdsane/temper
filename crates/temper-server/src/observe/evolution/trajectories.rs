use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::Deserialize;
use temper_runtime::scheduler::sim_now;
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

    // Query Turso directly (single source of truth).
    if let Some(turso) = state.persistent_store() {
        let tenant_str = tenant_scope.as_ref().map(|t| t.as_str().to_string());
        match turso
            .query_trajectory_stats(
                tenant_str.as_deref(),
                params.entity_type.as_deref(),
                params.action.as_deref(),
                success_filter,
                failed_limit as i64,
            )
            .await
        {
            Ok(stats) => {
                return Ok(Json(serde_json::json!({
                    "total": stats.total,
                    "success_count": stats.success_count,
                    "error_count": stats.error_count,
                    "success_rate": stats.success_rate,
                    "by_action": stats.by_action,
                    "failed_intents": stats.failed_intents,
                })));
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to query trajectories from Turso");
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
        }
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
        agent_id: None,
        session_id: None,
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
        intent: Some(intent.to_string()),
    };
    state
        .persist_trajectory_entry(&entry)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(StatusCode::CREATED)
}
