use std::collections::BTreeMap;
use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use axum::response::sse::{Event, KeepAlive, Sse};
use temper_evolution::FeatureRequestDisposition;
use temper_runtime::scheduler::sim_uuid;
use temper_runtime::tenant::TenantId;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::instrument;

use super::insight_generator;
use crate::authz::require_observe_auth;
use crate::request_context::AgentContext;
use crate::sentinel;
use crate::state::ServerState;

/// Persist an evolution record to Turso and return whether persistence succeeded.
async fn persist_evolution_record(
    state: &ServerState,
    record_id: &str,
    record_type: &str,
    status: &str,
    created_by: &str,
    derived_from: Option<&str>,
    data_json: &str,
) -> Result<(), String> {
    let Some(turso) = state.persistent_store() else {
        tracing::debug!(
            record_id,
            record_type,
            status,
            created_by,
            "evolution.store.unavailable"
        );
        return Ok(());
    };
    turso
        .insert_evolution_record(
            record_id,
            record_type,
            status,
            created_by,
            derived_from,
            data_json,
        )
        .await
        .map_err(|e| {
            tracing::warn!(
                record_id,
                record_type,
                status,
                created_by,
                error = %e,
                "evolution.store.write"
            );
            e.to_string()
        })?;
    tracing::info!(
        record_id,
        record_type,
        status,
        created_by,
        derived_from,
        "evolution.store.write"
    );
    Ok(())
}

/// Create an entity in the temper-system tenant, logging a warning on failure.
async fn create_system_entity(
    state: &ServerState,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: serde_json::Value,
) {
    let system_tenant = TenantId::new("temper-system");
    if let Err(e) = state
        .dispatch_tenant_action(
            &system_tenant,
            entity_type,
            entity_id,
            action,
            params,
            &AgentContext::system(),
        )
        .await
    {
        tracing::warn!(error = %e, entity_type, entity_id, "failed to create system entity");
    }
}

/// Persist sentinel alerts to Turso and create Observation entities.
async fn persist_alerts(
    state: &ServerState,
    alerts: &[sentinel::SentinelAlert],
) -> Result<Vec<serde_json::Value>, StatusCode> {
    let mut results = Vec::new();
    for alert in alerts {
        tracing::warn!(
            rule = %alert.rule_name,
            record_id = %alert.record.header.id,
            source = %alert.record.source,
            classification = ?alert.record.classification,
            observed_value = ?alert.record.observed_value,
            threshold = ?alert.record.threshold_value,
            "evolution.sentinel"
        );
        let data_json = serde_json::to_string(&alert.record).unwrap_or_default();
        if let Err(e) = persist_evolution_record(
            state,
            &alert.record.header.id,
            "Observation",
            &format!("{:?}", alert.record.header.status),
            &alert.record.header.created_by,
            alert.record.header.derived_from.as_deref(),
            &data_json,
        )
        .await
        {
            tracing::warn!(
                record_id = %alert.record.header.id,
                error = %e,
                "evolution.store.write"
            );
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }

        let obs_id = format!("OBS-{}", sim_uuid());
        create_system_entity(
            state,
            "Observation",
            &obs_id,
            "CreateObservation",
            serde_json::json!({
                "source": alert.record.source,
                "classification": format!("{:?}", alert.record.classification),
                "evidence_query": alert.record.evidence_query,
                "context": serde_json::to_string(&alert.record.context).unwrap_or_default(),
                "tenant": "temper-system",
                "legacy_record_id": alert.record.header.id,
            }),
        )
        .await;

        results.push(serde_json::json!({
            "rule": alert.rule_name,
            "record_id": alert.record.header.id,
            "entity_id": obs_id,
            "source": alert.record.source,
            "classification": alert.record.classification,
            "threshold": alert.record.threshold_value,
            "observed": alert.record.observed_value,
        }));
    }
    Ok(results)
}

/// Persist generated insights to Turso and create Insight entities.
async fn persist_insights(
    state: &ServerState,
    insights: &[temper_evolution::InsightRecord],
) -> Vec<serde_json::Value> {
    let mut results = Vec::new();
    for insight in insights {
        tracing::info!(
            record_id = %insight.header.id,
            category = ?insight.category,
            intent = %insight.signal.intent,
            volume = insight.signal.volume,
            success_rate = insight.signal.success_rate,
            priority_score = insight.priority_score,
            "evolution.insight"
        );
        let data_json = serde_json::to_string(insight).unwrap_or_default();
        if let Err(e) = persist_evolution_record(
            state,
            &insight.header.id,
            "Insight",
            &format!("{:?}", insight.header.status),
            &insight.header.created_by,
            insight.header.derived_from.as_deref(),
            &data_json,
        )
        .await
        {
            tracing::warn!(record_id = %insight.header.id, error = %e, "evolution.store.write");
        }

        let insight_id = format!("INS-{}", sim_uuid());
        create_system_entity(
            state,
            "Insight",
            &insight_id,
            "CreateInsight",
            serde_json::json!({
                "observation_id": "",
                "category": format!("{:?}", insight.category),
                "signal": insight.signal.intent,
                "recommendation": insight.recommendation,
                "priority_score": format!("{:.4}", insight.priority_score),
                "legacy_record_id": insight.header.id,
            }),
        )
        .await;

        results.push(serde_json::json!({
            "record_id": insight.header.id,
            "entity_id": insight_id,
            "category": format!("{:?}", insight.category),
            "intent": insight.signal.intent,
            "priority_score": insight.priority_score,
            "recommendation": insight.recommendation,
        }));
    }
    results
}

/// POST /api/evolution/sentinel/check -- trigger sentinel rule evaluation.
///
/// Evaluates all default sentinel rules against current server state.
/// Any triggered rules generate O-Records and store them in the RecordStore.
/// Returns a list of alerts (may be empty if all is healthy).
#[instrument(skip_all, fields(otel.name = "POST /api/evolution/sentinel/check"))]
pub(crate) async fn handle_sentinel_check(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "run_sentinel", "Evolution")?;
    let trajectory_entries = state.load_trajectory_entries(10_000).await;
    tracing::info!(
        trajectory_count = trajectory_entries.len(),
        "evolution.sentinel"
    );

    let rules = sentinel::default_rules();
    let alerts = sentinel::check_rules(&rules, &state, &trajectory_entries);
    if alerts.is_empty() {
        tracing::info!(rule_count = rules.len(), "evolution.sentinel");
    } else {
        tracing::warn!(
            rule_count = rules.len(),
            alerts_count = alerts.len(),
            "evolution.sentinel"
        );
    }
    let results = persist_alerts(&state, &alerts).await?;

    let insights = insight_generator::generate_insights(&trajectory_entries);
    tracing::info!(insights_count = insights.len(), "evolution.insight");
    let insight_results = persist_insights(&state, &insights).await;

    Ok(Json(serde_json::json!({
        "alerts_count": alerts.len(),
        "alerts": results,
        "insights_count": insights.len(),
        "insights": insight_results,
    })))
}

/// GET /observe/evolution/unmet-intents -- grouped unmet intents from trajectories.
///
/// Uses a SQL GROUP BY aggregation instead of loading raw trajectory rows to
/// avoid the OOM-causing bulk-load anti-pattern (previously 10,000 rows on
/// every 15-second Observe UI poll).
#[instrument(skip_all, fields(otel.name = "GET /observe/evolution/unmet-intents"))]
pub(crate) async fn handle_unmet_intents(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "read_evolution", "Evolution")?;
    let (failure_rows, submitted_specs) = state.load_unmet_intent_rows_aggregated().await;
    let intents =
        insight_generator::generate_unmet_intents_from_aggregated(&failure_rows, &submitted_specs);
    let open_count = intents.iter().filter(|i| i.status == "open").count();
    let resolved_count = intents.iter().filter(|i| i.status == "resolved").count();
    if open_count > 0 {
        tracing::warn!(
            open_count,
            resolved_count,
            total = intents.len(),
            "unmet_intent"
        );
    } else {
        tracing::info!(
            open_count,
            resolved_count,
            total = intents.len(),
            "unmet_intent"
        );
    }

    Ok(Json(serde_json::json!({
        "intents": intents,
        "open_count": open_count,
        "resolved_count": resolved_count,
    })))
}

/// GET /observe/evolution/feature-requests -- list feature request records from Turso.
///
/// Supports optional `disposition` query parameter to filter by status.
#[instrument(skip_all, fields(otel.name = "GET /observe/evolution/feature-requests"))]
pub(crate) async fn handle_feature_requests(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "read_evolution", "Evolution")?;
    let disposition_filter = params.get("disposition").map(|d| d.as_str());

    // Load trajectory entries for feature request generation.
    let trajectory_entries = state.load_trajectory_entries(10_000).await;

    // Query Turso directly (single source of truth).
    let system_tenant = TenantId::new("temper-system");
    if let Some(turso) = state.persistent_store() {
        // First, generate and upsert fresh feature requests from trajectory data.
        let generated = insight_generator::generate_feature_requests(&trajectory_entries);
        for fr in &generated {
            let refs_json =
                serde_json::to_string(&fr.trajectory_refs).unwrap_or_else(|_| "[]".to_string());
            let disp_str = match fr.disposition {
                FeatureRequestDisposition::Open => "Open",
                FeatureRequestDisposition::Acknowledged => "Acknowledged",
                FeatureRequestDisposition::Planned => "Planned",
                FeatureRequestDisposition::WontFix => "WontFix",
                FeatureRequestDisposition::Resolved => "Resolved",
            };
            if let Err(e) = turso
                .upsert_feature_request(
                    &fr.header.id,
                    &format!("{:?}", fr.category),
                    &fr.description,
                    fr.frequency as i64,
                    &refs_json,
                    disp_str,
                    fr.developer_notes.as_deref(),
                )
                .await
            {
                tracing::warn!(error = %e, "failed to upsert feature request to Turso");
            }

            // Also create FeatureRequest entity in temper-system tenant.
            let fr_id = format!("FR-{}", sim_uuid());
            let fr_params = serde_json::json!({
                "category": format!("{:?}", fr.category),
                "description": fr.description,
                "frequency": format!("{}", fr.frequency),
                "developer_notes": fr.developer_notes.clone().unwrap_or_default(),
                "legacy_record_id": fr.header.id,
            });
            if let Err(e) = state
                .dispatch_tenant_action(
                    &system_tenant,
                    "FeatureRequest",
                    &fr_id,
                    "CreateFeatureRequest",
                    fr_params,
                    &AgentContext::system(),
                )
                .await
            {
                tracing::warn!(error = %e, "failed to create FeatureRequest entity");
            }
        }

        // Then read back from Turso with filter.
        match turso.list_feature_requests(disposition_filter).await {
            Ok(rows) => {
                let items: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "category": r.category,
                            "description": r.description,
                            "frequency": r.frequency,
                            "trajectory_refs": serde_json::from_str::<serde_json::Value>(&r.trajectory_refs).unwrap_or_default(),
                            "disposition": r.disposition,
                            "developer_notes": r.developer_notes,
                            "created_at": r.created_at,
                        })
                    })
                    .collect();
                let total = items.len();
                return Ok(Json(
                    serde_json::json!({ "feature_requests": items, "total": total }),
                ));
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to query feature requests from Turso");
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
        }
    }

    // No persistent store configured — return empty.
    Ok(Json(
        serde_json::json!({ "feature_requests": [], "total": 0 }),
    ))
}

/// PATCH /observe/evolution/feature-requests/:id -- update disposition + notes in Turso.
///
/// Admin principals bypass Cedar; other principals require "manage_feature_requests"
/// on "FeatureRequest".
#[instrument(skip_all, fields(otel.name = "PATCH /observe/evolution/feature-requests/{id}"))]
pub(crate) async fn handle_update_feature_request(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Cedar authorization: admin/system bypass, others need manage_feature_requests.
    require_observe_auth(
        &state,
        &headers,
        "manage_feature_requests",
        "FeatureRequest",
    )?;

    let disposition = body.get("disposition").and_then(|v| v.as_str());
    let notes = body.get("developer_notes").and_then(|v| v.as_str());

    // Validate disposition if provided.
    if let Some(d) = disposition {
        match d.to_lowercase().as_str() {
            "open" | "acknowledged" | "planned" | "wontfix" | "wont_fix" | "resolved" => {}
            _ => {
                tracing::warn!(disposition = %d, "invalid disposition value");
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }

    let Some(turso) = state.persistent_store() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    turso
        .update_feature_request(&id, disposition.unwrap_or(""), notes)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to update feature request");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(serde_json::json!({
        "id": id,
        "updated": true,
    })))
}

/// GET /observe/evolution/stream -- SSE for real-time evolution events.
///
/// Streams new evolution records and insights as they are generated.
/// Uses the same broadcast channel pattern as the pending decision stream.
#[instrument(skip_all, fields(otel.name = "GET /observe/evolution/stream"))]
pub(crate) async fn handle_evolution_stream(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    require_observe_auth(&state, &headers, "read_evolution", "EvolutionStream")?;
    // Subscribe to pending decision broadcasts (which include authz denials
    // that create evolution records). A dedicated evolution broadcast channel
    // could be added later for O/P/A/D/I records specifically.
    let rx = state.pending_decision_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(pd) => Some(Ok(Event::default()
            .event("evolution_event")
            .json_data(serde_json::json!({
                "type": "new_decision",
                "decision_id": pd.id,
                "action": pd.action,
                "resource_type": pd.resource_type,
                "status": "pending",
            }))
            .unwrap_or_else(|_| Event::default().data("{}")))),
        Err(_) => None,
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
