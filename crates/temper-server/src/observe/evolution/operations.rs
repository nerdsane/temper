use std::collections::BTreeMap;
use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use axum::response::sse::{Event, KeepAlive, Sse};
use temper_evolution::FeatureRequestDisposition;
use temper_runtime::tenant::TenantId;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::instrument;

use super::insight_generator;
use crate::authz::require_observe_auth;
use crate::odata::extract_tenant;
use crate::request_context::AgentContext;
use crate::sentinel;
use crate::state::{ObserveRefreshHint, ServerState};

mod materialize;
mod support;

pub(crate) use materialize::{handle_evolution_analyze, handle_evolution_materialize};

use support::{
    create_system_entity_logged, emit_refresh_hints, next_system_entity_id, persist_alerts,
    persist_insights, spawn_intent_discovery,
};

/// POST /api/evolution/sentinel/check -- trigger sentinel rule evaluation.
///
/// Evaluates all default sentinel rules against current server state.
/// Any triggered rules generate O-Records and store them in the RecordStore.
/// Returns a list of alerts (may be empty if all is healthy).
#[instrument(skip_all, fields(
    otel.name = "POST /api/evolution/sentinel/check",
    trajectory_count = tracing::field::Empty,
    alerts_count = tracing::field::Empty,
    insights_count = tracing::field::Empty,
))]
pub(crate) async fn handle_sentinel_check(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "run_sentinel", "Evolution")?;
    let trajectory_entries = state.load_trajectory_entries(1_000).await;
    tracing::Span::current().record("trajectory_count", trajectory_entries.len());
    tracing::info!(
        trajectory_count = trajectory_entries.len(),
        "evolution.sentinel"
    );

    let rules = sentinel::default_rules();
    let alerts = sentinel::check_rules(&rules, &state, &trajectory_entries);
    tracing::Span::current().record("alerts_count", alerts.len());
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

    let analysis_tenant =
        extract_tenant(&headers, &state).unwrap_or_else(|_| TenantId::new("temper-system"));
    let mut discovery_results = Vec::new();
    let system_ctx = AgentContext::system();
    for alert in &alerts {
        let trigger_context = serde_json::json!({
            "rule_name": alert.rule_name.clone(),
            "observation_record_id": alert.record.header.id.clone(),
            "source": alert.record.source.clone(),
            "classification": format!("{:?}", alert.record.classification),
            "evidence_query": alert.record.evidence_query.clone(),
        });
        match spawn_intent_discovery(
            &state,
            &analysis_tenant,
            &format!("sentinel:{}", alert.rule_name),
            "automated",
            trigger_context,
            &system_ctx,
            false,
        )
        .await
        {
            Ok((entity_id, _)) => discovery_results.push(serde_json::json!({
                "entity_id": entity_id,
                "reason": format!("sentinel:{}", alert.rule_name),
            })),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    rule = %alert.rule_name,
                    "failed to create IntentDiscovery from sentinel"
                );
            }
        }
    }

    let insights = insight_generator::generate_insights(&trajectory_entries);
    tracing::Span::current().record("insights_count", insights.len());
    tracing::info!(insights_count = insights.len(), "evolution.insight");
    let insight_results = persist_insights(&state, &insights).await;

    emit_refresh_hints(
        &state,
        &[
            ObserveRefreshHint::EvolutionRecords,
            ObserveRefreshHint::EvolutionInsights,
            ObserveRefreshHint::UnmetIntents,
            ObserveRefreshHint::FeatureRequests,
        ],
    );

    Ok(Json(serde_json::json!({
        "alerts_count": alerts.len(),
        "alerts": results,
        "intent_discoveries": discovery_results,
        "insights_count": insights.len(),
        "insights": insight_results,
    })))
}

/// GET /observe/evolution/unmet-intents -- grouped unmet intents from trajectories.
///
/// Uses a SQL GROUP BY aggregation instead of loading raw trajectory rows to
/// avoid the OOM-causing bulk-load anti-pattern (previously 10,000 rows on
/// every 15-second Observe UI poll).
#[instrument(skip_all, fields(
    otel.name = "GET /observe/evolution/unmet-intents",
    open_count = tracing::field::Empty,
    resolved_count = tracing::field::Empty,
    total_intents = tracing::field::Empty,
))]
pub(crate) async fn handle_unmet_intents(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "read_evolution", "Evolution")?;
    let (failure_rows, submitted_specs) = state.load_unmet_intent_rows_aggregated().await;
    let intents =
        insight_generator::generate_unmet_intents_from_aggregated(&failure_rows, &submitted_specs);
    let open_count = intents
        .iter()
        .filter(|intent| intent.status == "open")
        .count();
    let resolved_count = intents
        .iter()
        .filter(|intent| intent.status == "resolved")
        .count();
    tracing::Span::current().record("open_count", open_count);
    tracing::Span::current().record("resolved_count", resolved_count);
    tracing::Span::current().record("total_intents", intents.len());
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

    for intent in &intents {
        tracing::debug!(
            entity_type = %intent.entity_type,
            action = %intent.action,
            error_pattern = %intent.error_pattern,
            failure_count = intent.failure_count,
            first_seen = %intent.first_seen,
            last_seen = %intent.last_seen,
            recommendation = %intent.recommendation,
            "unmet_intent.detail"
        );
    }

    Ok(Json(serde_json::json!({
        "intents": intents,
        "open_count": open_count,
        "resolved_count": resolved_count,
    })))
}

/// GET /observe/evolution/intent-evidence -- richer unmet-intent evidence from raw trajectories.
///
/// This endpoint is intentionally distinct from `/unmet-intents`. It uses a
/// bounded raw trajectory read so higher-level analysis can reason about
/// explicit caller intent, workaround sequences, and abandonment patterns
/// without changing the cheaper aggregated UI contract.
#[instrument(skip_all, fields(otel.name = "GET /observe/evolution/intent-evidence"))]
pub(crate) async fn handle_intent_evidence(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "read_evolution", "Evolution")?;
    let trajectory_entries = state.load_trajectory_entries(2_000).await;
    let evidence = insight_generator::generate_intent_evidence(&trajectory_entries);
    Ok(Json(serde_json::to_value(evidence).unwrap_or_else(|_| {
        serde_json::json!({
            "intent_candidates": [],
            "workaround_patterns": [],
            "abandonment_patterns": [],
            "trajectory_samples": [],
        })
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
    let disposition_filter = params.get("disposition").map(String::as_str);

    let trajectory_entries = state.load_trajectory_entries(1_000).await;

    if let Some(turso) = state.platform_persistent_store() {
        let generated = insight_generator::generate_feature_requests(&trajectory_entries);
        for feature_request in &generated {
            let refs_json = serde_json::to_string(&feature_request.trajectory_refs)
                .unwrap_or_else(|_| "[]".to_string());
            let disposition = match feature_request.disposition {
                FeatureRequestDisposition::Open => "Open",
                FeatureRequestDisposition::Acknowledged => "Acknowledged",
                FeatureRequestDisposition::Planned => "Planned",
                FeatureRequestDisposition::WontFix => "WontFix",
                FeatureRequestDisposition::Resolved => "Resolved",
            };
            if let Err(error) = turso
                .upsert_feature_request(
                    &feature_request.header.id,
                    &format!("{:?}", feature_request.category),
                    &feature_request.description,
                    feature_request.frequency as i64,
                    &refs_json,
                    disposition,
                    feature_request.developer_notes.as_deref(),
                )
                .await
            {
                tracing::warn!(error = %error, "failed to upsert feature request to Turso");
            }

            create_system_entity_logged(
                &state,
                "FeatureRequest",
                &next_system_entity_id("FR"),
                "CreateFeatureRequest",
                serde_json::json!({
                    "category": format!("{:?}", feature_request.category),
                    "description": feature_request.description,
                    "frequency": feature_request.frequency.to_string(),
                    "developer_notes": feature_request.developer_notes.clone().unwrap_or_default(),
                    "legacy_record_id": feature_request.header.id,
                }),
            )
            .await;
        }

        return match turso.list_feature_requests(disposition_filter).await {
            Ok(rows) => {
                let feature_requests = rows
                    .iter()
                    .map(|row| {
                        serde_json::json!({
                            "id": row.id,
                            "category": row.category,
                            "description": row.description,
                            "frequency": row.frequency,
                            "trajectory_refs": serde_json::from_str::<serde_json::Value>(&row.trajectory_refs).unwrap_or_default(),
                            "disposition": row.disposition,
                            "developer_notes": row.developer_notes,
                            "created_at": row.created_at,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(Json(serde_json::json!({
                    "feature_requests": feature_requests,
                    "total": feature_requests.len(),
                })))
            }
            Err(error) => {
                tracing::warn!(error = %error, "failed to query feature requests from Turso");
                Err(StatusCode::SERVICE_UNAVAILABLE)
            }
        };
    }

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
    require_observe_auth(
        &state,
        &headers,
        "manage_feature_requests",
        "FeatureRequest",
    )?;

    let disposition = body.get("disposition").and_then(serde_json::Value::as_str);
    let notes = body
        .get("developer_notes")
        .and_then(serde_json::Value::as_str);

    if let Some(value) = disposition {
        match value.to_lowercase().as_str() {
            "open" | "acknowledged" | "planned" | "wontfix" | "wont_fix" | "resolved" => {}
            _ => {
                tracing::warn!(disposition = %value, "invalid disposition value");
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }

    let Some(turso) = state.platform_persistent_store() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    turso
        .update_feature_request(&id, disposition.unwrap_or(""), notes)
        .await
        .map_err(|error| {
            tracing::error!(error = %error, "failed to update feature request");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    emit_refresh_hints(&state, &[ObserveRefreshHint::FeatureRequests]);

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
    let rx = state.pending_decision_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(pending_decision) => Some(Ok(Event::default()
            .event("evolution_event")
            .json_data(serde_json::json!({
                "type": "new_decision",
                "decision_id": pending_decision.id,
                "action": pending_decision.action,
                "resource_type": pending_decision.resource_type,
                "status": "pending",
            }))
            .unwrap_or_else(|_| Event::default().data("{}")))),
        Err(_) => None,
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
