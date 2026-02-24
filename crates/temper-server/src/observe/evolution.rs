//! Evolution engine endpoints: trajectories, sentinel checks, and O-P-A-D-I record management.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use temper_runtime::scheduler::sim_now;

use temper_evolution::{
    Decision, DecisionRecord, RecordHeader, RecordStatus, RecordType, validate_chain,
};

use crate::sentinel;
use crate::state::{ServerState, TrajectoryEntry};

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
            "SELECT COUNT(*) AS total, \
                    COALESCE(SUM(CASE WHEN success THEN 1 ELSE 0 END), 0) AS success_count \
             FROM trajectories \
             WHERE ($1::text IS NULL OR entity_type = $1) \
               AND ($2::text IS NULL OR action = $2) \
               AND ($3::bool IS NULL OR success = $3)",
        )
        .bind(params.entity_type.as_deref())
        .bind(params.action.as_deref())
        .bind(success_filter)
        .fetch_one(pool)
        .await;

        let by_action_rows: Result<Vec<(String, i64, i64, i64)>, sqlx::Error> = sqlx::query_as(
            "SELECT action, \
                    COUNT(*) AS total, \
                    COALESCE(SUM(CASE WHEN success THEN 1 ELSE 0 END), 0) AS success, \
                    COALESCE(SUM(CASE WHEN NOT success THEN 1 ELSE 0 END), 0) AS error \
             FROM trajectories \
             WHERE ($1::text IS NULL OR entity_type = $1) \
               AND ($2::text IS NULL OR action = $2) \
               AND ($3::bool IS NULL OR success = $3) \
             GROUP BY action \
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
            "SELECT tenant, entity_type, entity_id, action, from_status, error, created_at \
             FROM trajectories \
             WHERE success = false \
               AND ($1::text IS NULL OR entity_type = $1) \
               AND ($2::text IS NULL OR action = $2) \
               AND ($3::bool IS NULL OR success = $3) \
             ORDER BY created_at DESC \
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
    use temper_runtime::scheduler::sim_now;

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

    let entry = crate::state::TrajectoryEntry {
        timestamp: sim_now().to_rfc3339(),
        tenant: tenant.to_string(),
        entity_type: entity_type.to_string(),
        entity_id: "".to_string(),
        action: intent.to_string(),
        success: false,
        from_status: None,
        to_status: None,
        error: Some(if error_msg.is_empty() {
            format!("Unmet intent: {intent}")
        } else {
            error_msg.to_string()
        }),
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

// ---------------------------------------------------------------------------
// Phase 4: Sentinel Anomaly Detection
// ---------------------------------------------------------------------------

/// POST /api/evolution/sentinel/check -- trigger sentinel rule evaluation.
///
/// Evaluates all default sentinel rules against current server state.
/// Any triggered rules generate O-Records and store them in the RecordStore.
/// Returns a list of alerts (may be empty if all is healthy).
pub(crate) async fn handle_sentinel_check(
    State(state): State<ServerState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let rules = sentinel::default_rules();
    let alerts = sentinel::check_rules(&rules, &state);

    // Store generated O-Records.
    let mut results = Vec::new();
    for alert in &alerts {
        if let Some(ref pg_store) = state.pg_record_store {
            pg_store
                .insert_observation(&alert.record)
                .await
                .map_err(|e| {
                    tracing::error!(
                        record_id = %alert.record.header.id,
                        error = %e,
                        "failed to persist sentinel observation to postgres"
                    );
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
        }
        state.record_store.insert_observation(alert.record.clone());
        results.push(serde_json::json!({
            "rule": alert.rule_name,
            "record_id": alert.record.header.id,
            "source": alert.record.source,
            "classification": alert.record.classification,
            "threshold": alert.record.threshold_value,
            "observed": alert.record.observed_value,
        }));
    }

    Ok(Json(serde_json::json!({
        "alerts_count": alerts.len(),
        "alerts": results,
    })))
}

// ---------------------------------------------------------------------------
// Phase 5: Evolution Engine API
// ---------------------------------------------------------------------------

/// Query parameters for listing evolution records.
#[derive(Deserialize)]
pub(crate) struct EvolutionRecordParams {
    /// Filter by record type: "observation", "problem", "analysis", "decision", "insight".
    pub record_type: Option<String>,
    /// Filter by status: "open", "resolved", "superseded", "rejected".
    pub status: Option<String>,
}

/// GET /observe/evolution/records -- list all evolution records.
pub(crate) async fn list_evolution_records(
    State(state): State<ServerState>,
    Query(params): Query<EvolutionRecordParams>,
) -> Json<serde_json::Value> {
    if let Some(ref pg_store) = state.pg_record_store {
        let mut records: Vec<serde_json::Value> = Vec::new();
        let type_filter = params.record_type.as_deref();
        let status_filter = params.status.as_deref().and_then(parse_record_status);

        if (type_filter.is_none() || type_filter == Some("observation"))
            && let Ok(observations) = pg_store.open_observations().await
        {
            for obs in observations {
                if matches_status_filter(&obs.header.status, &status_filter) {
                    records.push(serde_json::json!({
                        "id": obs.header.id,
                        "record_type": "Observation",
                        "status": format!("{:?}", obs.header.status),
                        "created_by": obs.header.created_by,
                        "timestamp": obs.header.timestamp.to_rfc3339(),
                        "source": obs.source,
                        "classification": obs.classification,
                    }));
                }
            }
        }

        if (type_filter.is_none() || type_filter == Some("insight"))
            && let Ok(insights) = pg_store.ranked_insights().await
        {
            for insight in insights {
                if matches_status_filter(&insight.header.status, &status_filter) {
                    records.push(serde_json::json!({
                        "id": insight.header.id,
                        "record_type": "Insight",
                        "status": format!("{:?}", insight.header.status),
                        "created_by": insight.header.created_by,
                        "timestamp": insight.header.timestamp.to_rfc3339(),
                        "category": insight.category,
                        "priority_score": insight.priority_score,
                        "recommendation": insight.recommendation,
                    }));
                }
            }
        }

        let total_observations = pg_store.count(RecordType::Observation).await.unwrap_or(0);
        let total_problems = pg_store.count(RecordType::Problem).await.unwrap_or(0);
        let total_analyses = pg_store.count(RecordType::Analysis).await.unwrap_or(0);
        let total_decisions = pg_store.count(RecordType::Decision).await.unwrap_or(0);
        let total_insights = pg_store.count(RecordType::Insight).await.unwrap_or(0);

        if (type_filter.is_none() || type_filter == Some("problem")) && total_problems > 0 {
            records.push(serde_json::json!({
                "record_type": "Problem",
                "count": total_problems,
                "note": "Use GET /observe/evolution/records/{id} for individual records",
            }));
        }
        if (type_filter.is_none() || type_filter == Some("analysis")) && total_analyses > 0 {
            records.push(serde_json::json!({
                "record_type": "Analysis",
                "count": total_analyses,
                "note": "Use GET /observe/evolution/records/{id} for individual records",
            }));
        }
        if (type_filter.is_none() || type_filter == Some("decision")) && total_decisions > 0 {
            records.push(serde_json::json!({
                "record_type": "Decision",
                "count": total_decisions,
                "note": "Use GET /observe/evolution/records/{id} for individual records",
            }));
        }

        return Json(serde_json::json!({
            "records": records,
            "total_observations": total_observations,
            "total_problems": total_problems,
            "total_analyses": total_analyses,
            "total_decisions": total_decisions,
            "total_insights": total_insights,
        }));
    }

    let store = &state.record_store;
    let mut records: Vec<serde_json::Value> = Vec::new();

    let type_filter = params.record_type.as_deref();
    let status_filter = params.status.as_deref().and_then(parse_record_status);

    // Collect from each record type (only those matching the filter).
    if type_filter.is_none() || type_filter == Some("observation") {
        for obs in store.open_observations() {
            if matches_status_filter(&obs.header.status, &status_filter) {
                records.push(serde_json::json!({
                    "id": obs.header.id,
                    "record_type": "Observation",
                    "status": format!("{:?}", obs.header.status),
                    "created_by": obs.header.created_by,
                    "timestamp": obs.header.timestamp.to_rfc3339(),
                    "source": obs.source,
                    "classification": obs.classification,
                }));
            }
        }
    }

    if type_filter.is_none() || type_filter == Some("insight") {
        for insight in store.ranked_insights() {
            if matches_status_filter(&insight.header.status, &status_filter) {
                records.push(serde_json::json!({
                    "id": insight.header.id,
                    "record_type": "Insight",
                    "status": format!("{:?}", insight.header.status),
                    "created_by": insight.header.created_by,
                    "timestamp": insight.header.timestamp.to_rfc3339(),
                    "category": insight.category,
                    "priority_score": insight.priority_score,
                    "recommendation": insight.recommendation,
                }));
            }
        }
    }

    // For types not exposed via aggregation methods, use count as summary.
    if type_filter.is_none() || type_filter == Some("problem") {
        let count = store.count(RecordType::Problem);
        if count > 0 {
            records.push(serde_json::json!({
                "record_type": "Problem",
                "count": count,
                "note": "Use GET /observe/evolution/records/{id} for individual records",
            }));
        }
    }
    if type_filter.is_none() || type_filter == Some("analysis") {
        let count = store.count(RecordType::Analysis);
        if count > 0 {
            records.push(serde_json::json!({
                "record_type": "Analysis",
                "count": count,
                "note": "Use GET /observe/evolution/records/{id} for individual records",
            }));
        }
    }
    if type_filter.is_none() || type_filter == Some("decision") {
        let count = store.count(RecordType::Decision);
        if count > 0 {
            records.push(serde_json::json!({
                "record_type": "Decision",
                "count": count,
                "note": "Use GET /observe/evolution/records/{id} for individual records",
            }));
        }
    }

    Json(serde_json::json!({
        "records": records,
        "total_observations": store.count(RecordType::Observation),
        "total_problems": store.count(RecordType::Problem),
        "total_analyses": store.count(RecordType::Analysis),
        "total_decisions": store.count(RecordType::Decision),
        "total_insights": store.count(RecordType::Insight),
    }))
}

/// GET /observe/evolution/records/{id} -- get a single record with chain info.
pub(crate) async fn get_evolution_record(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if let Some(ref pg_store) = state.pg_record_store {
        if let Ok(Some(obs)) = pg_store.get_observation(&id).await {
            let chain = validate_chain_pg(pg_store, &id).await;
            return Ok(Json(serde_json::json!({
                "record": obs,
                "chain": {
                    "is_valid": chain.is_valid,
                    "chain_length": chain.chain_length,
                    "errors": chain.errors,
                },
            })));
        }
        if let Ok(Some(problem)) = pg_store.get_problem(&id).await {
            let chain = validate_chain_pg(pg_store, &id).await;
            return Ok(Json(serde_json::json!({
                "record": problem,
                "chain": {
                    "is_valid": chain.is_valid,
                    "chain_length": chain.chain_length,
                    "errors": chain.errors,
                },
            })));
        }
        if let Ok(Some(analysis)) = pg_store.get_analysis(&id).await {
            let chain = validate_chain_pg(pg_store, &id).await;
            return Ok(Json(serde_json::json!({
                "record": analysis,
                "chain": {
                    "is_valid": chain.is_valid,
                    "chain_length": chain.chain_length,
                    "errors": chain.errors,
                },
            })));
        }
        if let Ok(Some(decision)) = pg_store.get_decision(&id).await {
            let chain = validate_chain_pg(pg_store, &id).await;
            return Ok(Json(serde_json::json!({
                "record": decision,
                "chain": {
                    "is_valid": chain.is_valid,
                    "chain_length": chain.chain_length,
                    "errors": chain.errors,
                },
            })));
        }
        if let Ok(Some(insight)) = pg_store.get_insight(&id).await {
            let chain = validate_chain_pg(pg_store, &id).await;
            return Ok(Json(serde_json::json!({
                "record": insight,
                "chain": {
                    "is_valid": chain.is_valid,
                    "chain_length": chain.chain_length,
                    "errors": chain.errors,
                },
            })));
        }
    }

    let store = &state.record_store;

    // Try each record type.
    if let Some(obs) = store.get_observation(&id) {
        let chain = validate_chain(store, &id);
        return Ok(Json(serde_json::json!({
            "record": obs,
            "chain": {
                "is_valid": chain.is_valid,
                "chain_length": chain.chain_length,
                "errors": chain.errors,
            },
        })));
    }
    if let Some(problem) = store.get_problem(&id) {
        let chain = validate_chain(store, &id);
        return Ok(Json(serde_json::json!({
            "record": problem,
            "chain": {
                "is_valid": chain.is_valid,
                "chain_length": chain.chain_length,
                "errors": chain.errors,
            },
        })));
    }
    if let Some(analysis) = store.get_analysis(&id) {
        let chain = validate_chain(store, &id);
        return Ok(Json(serde_json::json!({
            "record": analysis,
            "chain": {
                "is_valid": chain.is_valid,
                "chain_length": chain.chain_length,
                "errors": chain.errors,
            },
        })));
    }
    if let Some(decision) = store.get_decision(&id) {
        let chain = validate_chain(store, &id);
        return Ok(Json(serde_json::json!({
            "record": decision,
            "chain": {
                "is_valid": chain.is_valid,
                "chain_length": chain.chain_length,
                "errors": chain.errors,
            },
        })));
    }
    if let Some(insight) = store.get_insight(&id) {
        let chain = validate_chain(store, &id);
        return Ok(Json(serde_json::json!({
            "record": insight,
            "chain": {
                "is_valid": chain.is_valid,
                "chain_length": chain.chain_length,
                "errors": chain.errors,
            },
        })));
    }

    Err(StatusCode::NOT_FOUND)
}

/// Request body for the decide endpoint.
#[derive(Deserialize)]
pub(crate) struct DecideRequest {
    /// The decision: "approved", "rejected", or "deferred".
    pub decision: String,
    /// Who is making the decision (email or identifier).
    pub decided_by: String,
    /// Human rationale for the decision.
    pub rationale: String,
}

/// POST /api/evolution/records/{id}/decide -- create a D-Record for a record.
///
/// The target record (by ID) must exist. Creates a DecisionRecord derived from it.
pub(crate) async fn handle_decide(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(body): Json<DecideRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let store = &state.record_store;

    // Verify the target record exists.
    let exists = if let Some(ref pg_store) = state.pg_record_store {
        pg_store
            .get_observation(&id)
            .await
            .map_err(|e| {
                tracing::error!(record_id = %id, error = %e, "failed to lookup observation in postgres");
                StatusCode::INTERNAL_SERVER_ERROR
            })?
            .is_some()
            || pg_store
                .get_problem(&id)
                .await
                .map_err(|e| {
                    tracing::error!(record_id = %id, error = %e, "failed to lookup problem in postgres");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
                .is_some()
            || pg_store
                .get_analysis(&id)
                .await
                .map_err(|e| {
                    tracing::error!(record_id = %id, error = %e, "failed to lookup analysis in postgres");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
                .is_some()
            || pg_store
                .get_decision(&id)
                .await
                .map_err(|e| {
                    tracing::error!(record_id = %id, error = %e, "failed to lookup decision in postgres");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
                .is_some()
            || pg_store
                .get_insight(&id)
                .await
                .map_err(|e| {
                    tracing::error!(record_id = %id, error = %e, "failed to lookup insight in postgres");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
                .is_some()
    } else {
        store.get_observation(&id).is_some()
            || store.get_problem(&id).is_some()
            || store.get_analysis(&id).is_some()
            || store.get_decision(&id).is_some()
            || store.get_insight(&id).is_some()
    };

    if !exists {
        return Err(StatusCode::NOT_FOUND);
    }

    let decision = match body.decision.to_lowercase().as_str() {
        "approved" | "approve" => Decision::Approved,
        "rejected" | "reject" => Decision::Rejected,
        "deferred" | "defer" => Decision::Deferred,
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    // Build the D-Record with DST-safe timestamps.
    let now = sim_now();
    let id_suffix = &temper_runtime::scheduler::sim_uuid().to_string()[..8];
    let year = now.format("%Y");
    let record_id = format!("D-{year}-{id_suffix}");

    let d_record = DecisionRecord {
        header: RecordHeader {
            id: record_id.clone(),
            record_type: RecordType::Decision,
            timestamp: now,
            created_by: body.decided_by.clone(),
            derived_from: Some(id.clone()),
            status: RecordStatus::Open,
        },
        decision,
        decided_by: body.decided_by,
        rationale: body.rationale,
        verification_results: None,
        implementation: None,
    };

    if let Some(ref pg_store) = state.pg_record_store {
        pg_store.insert_decision(&d_record).await.map_err(|e| {
            tracing::error!(record_id = %record_id, error = %e, "failed to persist decision record");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }
    store.insert_decision(d_record.clone());

    Ok(Json(serde_json::json!({
        "record_id": record_id,
        "decision": format!("{:?}", d_record.decision),
        "derived_from": id,
        "status": "Open",
    })))
}

/// GET /observe/evolution/insights -- list ranked insights (I-Records).
pub(crate) async fn list_evolution_insights(
    State(state): State<ServerState>,
) -> Json<serde_json::Value> {
    let insights = if let Some(ref pg_store) = state.pg_record_store {
        match pg_store.ranked_insights().await {
            Ok(items) => items,
            Err(e) => {
                tracing::warn!(error = %e, "failed to read insights from postgres, falling back to in-memory");
                state.record_store.ranked_insights()
            }
        }
    } else {
        state.record_store.ranked_insights()
    };

    let items: Vec<serde_json::Value> = insights
        .iter()
        .map(|i| {
            serde_json::json!({
                "id": i.header.id,
                "category": i.category,
                "priority_score": i.priority_score,
                "recommendation": i.recommendation,
                "signal": i.signal,
                "status": format!("{:?}", i.header.status),
                "timestamp": i.header.timestamp.to_rfc3339(),
            })
        })
        .collect();

    Json(serde_json::json!({
        "insights": items,
        "total": items.len(),
    }))
}

/// Parse a status filter string into a RecordStatus.
fn parse_record_status(s: &str) -> Option<RecordStatus> {
    match s.to_lowercase().as_str() {
        "open" => Some(RecordStatus::Open),
        "resolved" => Some(RecordStatus::Resolved),
        "superseded" => Some(RecordStatus::Superseded),
        "rejected" => Some(RecordStatus::Rejected),
        _ => None,
    }
}

/// Check if a record's status matches the optional filter.
fn matches_status_filter(status: &RecordStatus, filter: &Option<RecordStatus>) -> bool {
    match filter {
        Some(f) => status == f,
        None => true,
    }
}

/// Minimal chain validation summary used for Postgres-backed records.
#[derive(Debug)]
struct ChainValidationSummary {
    is_valid: bool,
    errors: Vec<String>,
    chain_length: usize,
}

fn record_type_from_id_prefix(id: &str) -> Option<RecordType> {
    if id.starts_with("O-") {
        Some(RecordType::Observation)
    } else if id.starts_with("P-") {
        Some(RecordType::Problem)
    } else if id.starts_with("A-") {
        Some(RecordType::Analysis)
    } else if id.starts_with("D-") {
        Some(RecordType::Decision)
    } else if id.starts_with("I-") {
        Some(RecordType::Insight)
    } else {
        None
    }
}

async fn fetch_pg_record_derived_from(
    store: &temper_evolution::PostgresRecordStore,
    id: &str,
    record_type: RecordType,
) -> Result<Option<String>, String> {
    match record_type {
        RecordType::Observation => store
            .get_observation(id)
            .await
            .map_err(|e| format!("failed to read Observation '{id}': {e}"))?
            .map(|r| r.header.derived_from)
            .ok_or_else(|| format!("record '{id}' not found")),
        RecordType::Problem => store
            .get_problem(id)
            .await
            .map_err(|e| format!("failed to read Problem '{id}': {e}"))?
            .map(|r| r.header.derived_from)
            .ok_or_else(|| format!("record '{id}' not found")),
        RecordType::Analysis => store
            .get_analysis(id)
            .await
            .map_err(|e| format!("failed to read Analysis '{id}': {e}"))?
            .map(|r| r.header.derived_from)
            .ok_or_else(|| format!("record '{id}' not found")),
        RecordType::Decision => store
            .get_decision(id)
            .await
            .map_err(|e| format!("failed to read Decision '{id}': {e}"))?
            .map(|r| r.header.derived_from)
            .ok_or_else(|| format!("record '{id}' not found")),
        RecordType::Insight => store
            .get_insight(id)
            .await
            .map_err(|e| format!("failed to read Insight '{id}': {e}"))?
            .map(|r| r.header.derived_from)
            .ok_or_else(|| format!("record '{id}' not found")),
    }
}

async fn validate_chain_pg(
    store: &temper_evolution::PostgresRecordStore,
    leaf_id: &str,
) -> ChainValidationSummary {
    let mut errors = Vec::new();
    let mut chain_length = 0usize;
    let mut current_id = leaf_id.to_string();
    let mut expected_types: Vec<RecordType> = Vec::new();

    loop {
        chain_length += 1;
        let Some(record_type) = record_type_from_id_prefix(&current_id) else {
            errors.push(format!("unknown record type prefix in '{current_id}'"));
            break;
        };

        if !expected_types.is_empty() && !expected_types.contains(&record_type) {
            errors.push(format!(
                "record '{current_id}' is {:?} but expected one of {:?}",
                record_type, expected_types
            ));
        }

        expected_types = match record_type {
            RecordType::Decision => vec![RecordType::Analysis],
            RecordType::Analysis => vec![RecordType::Problem],
            RecordType::Problem => vec![RecordType::Observation],
            RecordType::Observation => vec![],
            RecordType::Insight => vec![RecordType::Observation],
        };

        let derived_from = match fetch_pg_record_derived_from(store, &current_id, record_type).await
        {
            Ok(parent) => parent,
            Err(e) => {
                errors.push(e);
                break;
            }
        };

        match derived_from {
            Some(parent_id) => {
                current_id = parent_id;
            }
            None => {
                if record_type != RecordType::Observation && record_type != RecordType::Insight {
                    errors.push(format!(
                        "chain root '{current_id}' is {:?}, expected Observation",
                        record_type
                    ));
                }
                break;
            }
        }

        if chain_length > 100 {
            errors.push("chain exceeded maximum depth of 100".to_string());
            break;
        }
    }

    ChainValidationSummary {
        is_valid: errors.is_empty(),
        errors,
        chain_length,
    }
}
