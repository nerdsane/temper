use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use temper_authz::AuthenticatedRequestContext;
use tracing::instrument;

use crate::authz::{require_authenticated_context, require_observe_auth};
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
#[instrument(skip_all, fields(otel.name = "GET /observe/evolution/records"))]
pub(crate) async fn handle_list_evolution_records(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Query(params): Query<EvolutionRecordParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_evolution", "Evolution")?;

    match state
        .list_evolution_records(
            authenticated.tenant().as_str(),
            params.record_type.as_deref(),
            params.status.as_deref(),
        )
        .await
    {
        Ok(rows) => {
            let records: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    let mut val = serde_json::json!({
                        "id": r.id,
                        "record_type": r.record_type,
                        "status": r.status,
                        "created_by": r.created_by,
                        "timestamp": r.timestamp,
                    });
                    if let Some(ref df) = r.derived_from {
                        val["derived_from"] = serde_json::json!(df);
                    }
                    // Merge data fields into the response.
                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&r.data)
                        && let Some(obj) = data.as_object()
                    {
                        for (k, v) in obj {
                            val[k] = v.clone();
                        }
                    }
                    val
                })
                .collect();

            // Count by type.
            let count_type = |t: &str| rows.iter().filter(|r| r.record_type == t).count();
            Ok(Json(serde_json::json!({
                "records": records,
                "total_observations": count_type("Observation"),
                "total_problems": count_type("Problem"),
                "total_analyses": count_type("Analysis"),
                "total_decisions": count_type("Decision"),
                "total_insights": count_type("Insight"),
            })))
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to query evolution records");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// GET /observe/evolution/insights -- list ranked insights (I-Records).
#[instrument(skip_all, fields(otel.name = "GET /observe/evolution/insights"))]
pub(crate) async fn handle_list_evolution_insights(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_evolution", "Evolution")?;

    match state
        .list_ranked_insights(authenticated.tenant().as_str())
        .await
    {
        Ok(rows) => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    let mut val = serde_json::json!({
                        "id": r.id,
                        "status": r.status,
                        "timestamp": r.timestamp,
                    });
                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&r.data)
                        && let Some(obj) = data.as_object()
                    {
                        for (k, v) in obj {
                            val[k] = v.clone();
                        }
                    }
                    val
                })
                .collect();
            let total = items.len();
            Ok(Json(serde_json::json!({
                "insights": items,
                "total": total,
            })))
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to query insights");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}
