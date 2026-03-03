use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use temper_runtime::scheduler::sim_now;

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

/// GET /observe/trajectories -- aggregated trajectory stats with failed intent details.
///
/// Returns:
/// - `total`: total matching entries
/// - `success_count` / `error_count` / `success_rate`
/// - `by_action`: per-action breakdown
/// - `failed_intents`: most recent failed entries (up to `failed_limit`)
pub(crate) async fn handle_trajectories(
    State(state): State<ServerState>,
    Query(params): Query<TrajectoryQueryParams>,
) -> Json<serde_json::Value> {
    let failed_limit = params.failed_limit.unwrap_or(50).min(500);
    let success_filter: Option<bool> = params.success.as_deref().map(|s| s == "true");
    if let Some(pool) = state
        .event_store
        .as_ref()
        .and_then(|store| store.postgres_pool())
    {
        let totals: Result<(i64, i64), sqlx::Error> = sqlx::query_as(
            "SELECT COUNT(*) AS total, \\
                    COALESCE(SUM(CASE WHEN success THEN 1 ELSE 0 END), 0) AS success_count \\
             FROM trajectories \\
             WHERE ($1::text IS NULL OR entity_type = $1) \\
               AND ($2::text IS NULL OR action = $2) \\
               AND ($3::bool IS NULL OR success = $3)",
        )
        .bind(params.entity_type.as_deref())
        .bind(params.action.as_deref())
        .bind(success_filter)
        .fetch_one(pool)
        .await;

        let by_action_rows: Result<Vec<(String, i64, i64, i64)>, sqlx::Error> = sqlx::query_as(
            "SELECT action, \\
                    COUNT(*) AS total, \\
                    COALESCE(SUM(CASE WHEN success THEN 1 ELSE 0 END), 0) AS success, \\
                    COALESCE(SUM(CASE WHEN NOT success THEN 1 ELSE 0 END), 0) AS error \\
             FROM trajectories \\
             WHERE ($1::text IS NULL OR entity_type = $1) \\
               AND ($2::text IS NULL OR action = $2) \\
               AND ($3::bool IS NULL OR success = $3) \\
             GROUP BY action \\
             ORDER BY action ASC",
        )
        .bind(params.entity_type.as_deref())
        .bind(params.action.as_deref())
        .bind(success_filter)
        .fetch_all(pool)
        .await;

        type FailedRow = (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            chrono::DateTime<chrono::Utc>,
        );
        let failed_rows: Result<Vec<FailedRow>, sqlx::Error> = sqlx::query_as(
            "SELECT tenant, entity_type, entity_id, action, from_status, error, created_at \\
             FROM trajectories \\
             WHERE success = false \\
               AND ($1::text IS NULL OR entity_type = $1) \\
               AND ($2::text IS NULL OR action = $2) \\
               AND ($3::bool IS NULL OR success = $3) \\
             ORDER BY created_at DESC \\
             LIMIT $4",
        )
        .bind(params.entity_type.as_deref())
        .bind(params.action.as_deref())
        .bind(success_filter)
        .bind(failed_limit as i64)
        .fetch_all(pool)
        .await;

        if let (Ok((total, success_count)), Ok(by_action_rows), Ok(failed_rows)) =
            (totals, by_action_rows, failed_rows)
        {
            let total = total as u64;
            let success_count = success_count as u64;
            let error_count = total.saturating_sub(success_count);
            let success_rate = if total > 0 {
                success_count as f64 / total as f64
            } else {
                0.0
            };

            let by_action: std::collections::BTreeMap<String, serde_json::Value> = by_action_rows
                .into_iter()
                .map(|(action, total, success, error)| {
                    (
                        action,
                        serde_json::json!({
                            "total": total as u64,
                            "success": success as u64,
                            "error": error as u64,
                        }),
                    )
                })
                .collect();

            let failed_intents: Vec<serde_json::Value> = failed_rows
                .into_iter()
                .map(
                    |(tenant, entity_type, entity_id, action, from_status, error, created_at)| {
                        serde_json::json!({
                            "timestamp": created_at.to_rfc3339(),
                            "tenant": tenant,
                            "entity_type": entity_type,
                            "entity_id": entity_id,
                            "action": action,
                            "from_status": from_status,
                            "error": error,
                        })
                    },
                )
                .collect();

            return Json(serde_json::json!({
                "total": total,
                "success_count": success_count,
                "error_count": error_count,
                "success_rate": success_rate,
                "by_action": by_action,
                "failed_intents": failed_intents,
            }));
        }

        tracing::warn!("failed to query trajectories from postgres, falling back to in-memory log");
    }

    let log = state
        .trajectory_log
        .read()
        .unwrap_or_else(|e| e.into_inner());

    // Filter entries.
    let filtered: Vec<&TrajectoryEntry> = log
        .entries()
        .iter()
        .filter(|e| {
            if let Some(ref ft) = params.entity_type
                && e.entity_type != *ft
            {
                return false;
            }
            if let Some(ref fa) = params.action
                && e.action != *fa
            {
                return false;
            }
            if let Some(sf) = success_filter
                && e.success != sf
            {
                return false;
            }
            true
        })
        .collect();

    let total = filtered.len() as u64;
    let success_count = filtered.iter().filter(|e| e.success).count() as u64;
    let error_count = total.saturating_sub(success_count);
    let success_rate = if total > 0 {
        success_count as f64 / total as f64
    } else {
        0.0
    };

    // Per-action breakdown (BTreeMap for deterministic order).
    let mut by_action: std::collections::BTreeMap<String, serde_json::Value> =
        std::collections::BTreeMap::new();
    for entry in &filtered {
        let stats = by_action
            .entry(entry.action.clone())
            .or_insert_with(|| serde_json::json!({"total": 0u64, "success": 0u64, "error": 0u64}));
        if let Some(obj) = stats.as_object_mut() {
            *obj.entry("total").or_insert(serde_json::json!(0)) =
                serde_json::json!(obj["total"].as_u64().unwrap_or(0) + 1);
            if entry.success {
                *obj.entry("success").or_insert(serde_json::json!(0)) =
                    serde_json::json!(obj["success"].as_u64().unwrap_or(0) + 1);
            } else {
                *obj.entry("error").or_insert(serde_json::json!(0)) =
                    serde_json::json!(obj["error"].as_u64().unwrap_or(0) + 1);
            }
        }
    }

    // Collect most recent failed intents.
    let failed_intents: Vec<serde_json::Value> = filtered
        .iter()
        .rev()
        .filter(|e| !e.success)
        .take(failed_limit)
        .map(|e| {
            serde_json::json!({
                "timestamp": e.timestamp,
                "tenant": e.tenant,
                "entity_type": e.entity_type,
                "entity_id": e.entity_id,
                "action": e.action,
                "from_status": e.from_status,
                "error": e.error,
            })
        })
        .collect();

    Json(serde_json::json!({
        "total": total,
        "success_count": success_count,
        "error_count": error_count,
        "success_rate": success_rate,
        "by_action": by_action,
        "failed_intents": failed_intents,
    }))
}

/// POST /api/evolution/trajectories/unmet -- record an unmet user intent.
///
/// Called by the production chat proxy when a user asks for something
/// that doesn't map to any available action. This feeds the Evolution Engine.
pub(crate) async fn handle_unmet_intent(
    State(state): State<ServerState>,
    Json(body): Json<serde_json::Value>,
) -> Result<StatusCode, (StatusCode, String)> {
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
    };
    state
        .persist_trajectory_entry(&entry)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if let Ok(mut log) = state.trajectory_log.write() {
        log.push(entry.clone());
    }

    Ok(StatusCode::CREATED)
}
