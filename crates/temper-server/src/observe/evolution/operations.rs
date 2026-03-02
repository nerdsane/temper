use std::collections::BTreeMap;
use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::response::sse::{Event, KeepAlive, Sse};
use temper_evolution::FeatureRequestDisposition;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::insight_generator;
use crate::sentinel;
use crate::state::ServerState;

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

    // Also generate insights from trajectory data.
    let insights = insight_generator::generate_insights(&state);
    let mut insight_results = Vec::new();
    for insight in &insights {
        state.record_store.insert_insight(insight.clone());
        if let Some(ref pg_store) = state.pg_record_store
            && let Err(e) = pg_store.insert_insight(insight).await
        {
            tracing::error!(
                record_id = %insight.header.id,
                error = %e,
                "failed to persist insight to postgres"
            );
        }
        insight_results.push(serde_json::json!({
            "record_id": insight.header.id,
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
pub(crate) async fn handle_unmet_intents(
    State(state): State<ServerState>,
) -> Json<serde_json::Value> {
    let intents = insight_generator::generate_unmet_intents(&state);
    let open_count = intents.iter().filter(|i| i.status == "open").count();
    let resolved_count = intents.iter().filter(|i| i.status == "resolved").count();

    Json(serde_json::json!({
        "intents": intents,
        "open_count": open_count,
        "resolved_count": resolved_count,
    }))
}

/// GET /observe/evolution/feature-requests -- list feature request records.
///
/// Supports optional `disposition` query parameter to filter by status.
pub(crate) async fn handle_feature_requests(
    State(state): State<ServerState>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Json<serde_json::Value> {
    // Generate fresh feature requests from trajectory data.
    let generated = insight_generator::generate_feature_requests(&state);

    // Merge with stored feature requests (update frequencies, add new ones).
    {
        let mut log = state.feature_request_log.write().unwrap(); // ci-ok: infallible lock
        for fr in generated {
            // Dedup by category + stable action/prefix key.
            let fr_key = insight_generator::feature_request_dedup_key(&fr.description);
            let existing = log.iter_mut().find(|existing| {
                existing.category == fr.category
                    && insight_generator::feature_request_dedup_key(&existing.description) == fr_key
            });
            if let Some(existing) = existing {
                existing.frequency = fr.frequency;
                existing.trajectory_refs = fr.trajectory_refs;
            } else if log.len() < 500 {
                log.push(fr);
            }
        }
    }

    // Read and optionally filter.
    let log = state.feature_request_log.read().unwrap(); // ci-ok: infallible lock
    let disposition_filter = params.get("disposition").map(|d| d.to_string());

    let filtered: Vec<serde_json::Value> = log
        .iter()
        .filter(|fr| {
            if let Some(ref filter) = disposition_filter {
                let disp_str = match fr.disposition {
                    FeatureRequestDisposition::Open => "Open",
                    FeatureRequestDisposition::Acknowledged => "Acknowledged",
                    FeatureRequestDisposition::Planned => "Planned",
                    FeatureRequestDisposition::WontFix => "WontFix",
                    FeatureRequestDisposition::Resolved => "Resolved",
                };
                disp_str.eq_ignore_ascii_case(filter)
            } else {
                true
            }
        })
        .map(|fr| {
            serde_json::json!({
                "id": fr.header.id,
                "category": fr.category,
                "description": fr.description,
                "frequency": fr.frequency,
                "trajectory_refs": &fr.trajectory_refs[..fr.trajectory_refs.len().min(100)],
                "disposition": fr.disposition,
                "developer_notes": fr.developer_notes,
                "created_at": fr.header.timestamp.to_rfc3339(),
            })
        })
        .collect();

    Json(serde_json::json!(filtered))
}

/// PATCH /observe/evolution/feature-requests/:id -- update disposition + notes.
pub(crate) async fn handle_update_feature_request(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let mut log = state.feature_request_log.write().unwrap(); // ci-ok: infallible lock

    let fr = log
        .iter_mut()
        .find(|fr| fr.header.id == id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("Feature request '{id}' not found"),
            )
        })?;

    if let Some(disposition) = body.get("disposition").and_then(|v| v.as_str()) {
        fr.disposition = match disposition.to_lowercase().as_str() {
            "open" => FeatureRequestDisposition::Open,
            "acknowledged" => FeatureRequestDisposition::Acknowledged,
            "planned" => FeatureRequestDisposition::Planned,
            "wontfix" | "wont_fix" => FeatureRequestDisposition::WontFix,
            "resolved" => FeatureRequestDisposition::Resolved,
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("Invalid disposition: {disposition}"),
                ));
            }
        };
    }

    if let Some(notes) = body.get("developer_notes").and_then(|v| v.as_str()) {
        fr.developer_notes = Some(notes.to_string());
    }

    let updated = serde_json::to_value(&*fr).unwrap_or_default();
    Ok(Json(updated))
}

/// GET /observe/evolution/stream -- SSE for real-time evolution events.
///
/// Streams new evolution records and insights as they are generated.
/// Uses the same broadcast channel pattern as the pending decision stream.
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
