use std::collections::BTreeMap;
use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::response::sse::{Event, KeepAlive, Sse};
use temper_evolution::FeatureRequestDisposition;
use temper_runtime::scheduler::sim_uuid;
use temper_runtime::tenant::TenantId;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::instrument;

use crate::dispatch::AgentContext;
use crate::insight_generator;
use crate::sentinel;
use crate::state::ServerState;

/// POST /api/evolution/sentinel/check -- trigger sentinel rule evaluation.
///
/// Evaluates all default sentinel rules against current server state.
/// Any triggered rules generate O-Records and store them in the RecordStore.
/// Returns a list of alerts (may be empty if all is healthy).
#[instrument(skip_all, fields(otel.name = "POST /api/evolution/sentinel/check"))]
pub(crate) async fn handle_sentinel_check(
    State(state): State<ServerState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Load trajectory entries from Turso for sentinel and insight generation.
    let trajectory_entries = state.load_trajectory_entries(10_000).await;

    let rules = sentinel::default_rules();
    let alerts = sentinel::check_rules(&rules, &state, &trajectory_entries);

    // Store generated O-Records and create Observation entities.
    let system_tenant = TenantId::new("temper-system");
    let mut results = Vec::new();
    for alert in &alerts {
        // Persist observation to Turso.
        if let Some(turso) = state.turso_opt() {
            let data_json = serde_json::to_string(&alert.record).unwrap_or_default();
            if let Err(e) = turso
                .insert_evolution_record(
                    &alert.record.header.id,
                    "Observation",
                    &format!("{:?}", alert.record.header.status),
                    &alert.record.header.created_by,
                    alert.record.header.derived_from.as_deref(),
                    &data_json,
                )
                .await
            {
                tracing::error!(
                    record_id = %alert.record.header.id,
                    error = %e,
                    "failed to persist sentinel observation to Turso"
                );
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }

        // Create Observation entity in temper-system tenant.
        let obs_id = format!("OBS-{}", sim_uuid());
        let obs_params = serde_json::json!({
            "source": alert.record.source,
            "classification": format!("{:?}", alert.record.classification),
            "evidence_query": alert.record.evidence_query,
            "context": serde_json::to_string(&alert.record.context).unwrap_or_default(),
            "tenant": "temper-system",
            "legacy_record_id": alert.record.header.id,
        });
        if let Err(e) = state
            .dispatch_tenant_action(
                &system_tenant,
                "Observation",
                &obs_id,
                "CreateObservation",
                obs_params,
                &AgentContext::default(),
            )
            .await
        {
            tracing::warn!(error = %e, "failed to create Observation entity for sentinel alert");
        }

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

    // Also generate insights from trajectory data.
    let insights = insight_generator::generate_insights(&trajectory_entries);
    let mut insight_results = Vec::new();
    for insight in &insights {
        // Persist insight to Turso.
        if let Some(turso) = state.turso_opt() {
            let data_json = serde_json::to_string(insight).unwrap_or_default();
            if let Err(e) = turso
                .insert_evolution_record(
                    &insight.header.id,
                    "Insight",
                    &format!("{:?}", insight.header.status),
                    &insight.header.created_by,
                    insight.header.derived_from.as_deref(),
                    &data_json,
                )
                .await
            {
                tracing::error!(
                    record_id = %insight.header.id,
                    error = %e,
                    "failed to persist insight to Turso"
                );
            }
        }

        // Create Insight entity in temper-system tenant.
        let insight_id = format!("INS-{}", sim_uuid());
        let insight_params = serde_json::json!({
            "observation_id": "",
            "category": format!("{:?}", insight.category),
            "signal": insight.signal.intent,
            "recommendation": insight.recommendation,
            "priority_score": format!("{:.4}", insight.priority_score),
            "legacy_record_id": insight.header.id,
        });
        if let Err(e) = state
            .dispatch_tenant_action(
                &system_tenant,
                "Insight",
                &insight_id,
                "CreateInsight",
                insight_params,
                &AgentContext::default(),
            )
            .await
        {
            tracing::warn!(error = %e, "failed to create Insight entity");
        }

        insight_results.push(serde_json::json!({
            "record_id": insight.header.id,
            "entity_id": insight_id,
            "category": format!("{:?}", insight.category),
            "intent": insight.signal.intent,
            "priority_score": insight.priority_score,
            "recommendation": insight.recommendation,
        }));
    }

    Ok(Json(serde_json::json!({
        "alerts_count": alerts.len(),
        "alerts": results,
        "insights_count": insights.len(),
        "insights": insight_results,
    })))
}

/// GET /observe/evolution/unmet-intents -- grouped unmet intents from trajectories.
#[instrument(skip_all, fields(otel.name = "GET /observe/evolution/unmet-intents"))]
pub(crate) async fn handle_unmet_intents(
    State(state): State<ServerState>,
) -> Json<serde_json::Value> {
    let trajectory_entries = state.load_trajectory_entries(10_000).await;
    let intents = insight_generator::generate_unmet_intents(&trajectory_entries);
    let open_count = intents.iter().filter(|i| i.status == "open").count();
    let resolved_count = intents.iter().filter(|i| i.status == "resolved").count();

    Json(serde_json::json!({
        "intents": intents,
        "open_count": open_count,
        "resolved_count": resolved_count,
    }))
}

/// GET /observe/evolution/feature-requests -- list feature request records from Turso.
///
/// Supports optional `disposition` query parameter to filter by status.
#[instrument(skip_all, fields(otel.name = "GET /observe/evolution/feature-requests"))]
pub(crate) async fn handle_feature_requests(
    State(state): State<ServerState>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Json<serde_json::Value> {
    let disposition_filter = params.get("disposition").map(|d| d.as_str());

    // Load trajectory entries for feature request generation.
    let trajectory_entries = state.load_trajectory_entries(10_000).await;

    // Query Turso directly (single source of truth).
    let system_tenant = TenantId::new("temper-system");
    if let Some(turso) = state.turso_opt() {
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
                    &AgentContext::default(),
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
                return Json(serde_json::json!(items));
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to query feature requests from Turso");
            }
        }
    }

    // Fallback: empty response when no Turso configured.
    Json(serde_json::json!([]))
}

/// PATCH /observe/evolution/feature-requests/:id -- update disposition + notes in Turso.
#[instrument(skip_all, fields(otel.name = "PATCH /observe/evolution/feature-requests/{id}"))]
pub(crate) async fn handle_update_feature_request(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let disposition = body.get("disposition").and_then(|v| v.as_str());
    let notes = body.get("developer_notes").and_then(|v| v.as_str());

    // Validate disposition if provided.
    if let Some(d) = disposition {
        match d.to_lowercase().as_str() {
            "open" | "acknowledged" | "planned" | "wontfix" | "wont_fix" | "resolved" => {}
            _ => {
                tracing::warn!(disposition = %d, "invalid disposition value");
                return Err((StatusCode::BAD_REQUEST, format!("Invalid disposition: {d}")));
            }
        }
    }

    let Some(turso) = state.turso_opt() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "Turso backend not configured".to_string(),
        ));
    };

    turso
        .update_feature_request(&id, disposition.unwrap_or(""), notes)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to update feature request: {e}"),
            )
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
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
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

    Sse::new(stream).keep_alive(KeepAlive::default())
}
