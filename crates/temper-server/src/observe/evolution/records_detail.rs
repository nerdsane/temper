use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use temper_evolution::{
    Decision, DecisionRecord, RecordHeader, RecordStatus, RecordType, validate_chain,
};
use temper_runtime::scheduler::sim_now;

use crate::state::ServerState;

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
    } else if id.starts_with("FR-") {
        Some(RecordType::FeatureRequest)
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
        RecordType::FeatureRequest => Ok(None), // FR-Records not stored in Postgres yet
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
            RecordType::FeatureRequest => vec![RecordType::Insight],
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
