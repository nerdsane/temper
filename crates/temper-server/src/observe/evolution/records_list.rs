use axum::extract::{Query, State};
use axum::response::Json;
use serde::Deserialize;
use temper_evolution::{RecordStatus, RecordType};

use crate::state::ServerState;

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
