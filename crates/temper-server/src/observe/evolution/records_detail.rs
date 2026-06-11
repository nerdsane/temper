use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use serde::Deserialize;
use temper_authz::PrincipalKind;
use temper_evolution::{Decision, DecisionRecord, RecordHeader, RecordStatus, RecordType};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use crate::authz::{require_observe_auth, security_context_from_headers};
use crate::request_context::AgentContext;
use crate::state::ServerState;

/// GET /observe/evolution/records/{id} -- get a single record with chain info.
#[instrument(skip_all, fields(otel.name = "GET /observe/evolution/records/{id}"))]
pub(crate) async fn handle_get_evolution_record(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "read_evolution", "Evolution")?;

    match state.get_evolution_record(&id).await {
        Ok(Some(row)) => {
            let mut record: serde_json::Value =
                serde_json::from_str(&row.data).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = record.as_object_mut() {
                obj.insert("id".to_string(), serde_json::json!(row.id));
                obj.insert(
                    "record_type".to_string(),
                    serde_json::json!(row.record_type),
                );
                obj.insert("status".to_string(), serde_json::json!(row.status));
                obj.insert("created_by".to_string(), serde_json::json!(row.created_by));
                obj.insert("timestamp".to_string(), serde_json::json!(row.timestamp));
                if let Some(ref df) = row.derived_from {
                    obj.insert("derived_from".to_string(), serde_json::json!(df));
                }
            }
            let chain = validate_chain(&state, &id).await;
            Ok(Json(serde_json::json!({
                "record": record,
                "chain": {
                    "is_valid": chain.is_valid,
                    "chain_length": chain.chain_length,
                    "errors": chain.errors,
                },
            })))
        }
        Ok(None) => {
            tracing::warn!(record_id = %id, "evolution record not found");
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            tracing::error!(record_id = %id, error = %e, "failed to lookup evolution record");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
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
/// Admin principals bypass Cedar; other principals require "manage_decisions" on "EvolutionRecord".
#[instrument(skip_all, fields(otel.name = "POST /api/evolution/records/{id}/decide"))]
pub(crate) async fn handle_decide(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<DecideRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Cedar authorization: admin bypass, others need manage_decisions.
    let security_ctx = security_context_from_headers(&headers, None, None, None);
    let tenant_hint = headers
        .get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("system");
    if !matches!(security_ctx.principal.kind, PrincipalKind::Admin)
        && let Err(denial) = state.authorize_with_context(
            &security_ctx,
            "manage_decisions",
            "EvolutionRecord",
            &std::collections::BTreeMap::new(),
            tenant_hint,
        )
    {
        tracing::warn!(reason = %denial, "unauthorized decide attempt");
        return Err(StatusCode::FORBIDDEN);
    }

    // Verify the target record exists.
    let exists = state
        .get_evolution_record(&id)
        .await
        .map_err(|e| {
            tracing::error!(record_id = %id, error = %e, "failed to lookup record");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .is_some();

    if !exists {
        tracing::warn!(record_id = %id, "target record not found for decide");
        return Err(StatusCode::NOT_FOUND);
    }

    let decision = match body.decision.to_lowercase().as_str() {
        "approved" | "approve" => Decision::Approved,
        "rejected" | "reject" => Decision::Rejected,
        "deferred" | "defer" => Decision::Deferred,
        _ => {
            tracing::warn!(decision = %body.decision, "invalid decision value");
            return Err(StatusCode::BAD_REQUEST);
        }
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

    // Persist to the available evolution store.
    let data_json = serde_json::to_string(&d_record).unwrap_or_default();
    state
        .insert_evolution_record(
            &record_id,
            "Decision",
            &format!("{:?}", d_record.header.status),
            &d_record.header.created_by,
            d_record.header.derived_from.as_deref(),
            &data_json,
        )
        .await
        .map_err(|e| {
            tracing::error!(record_id = %record_id, error = %e, "failed to persist decision record");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Also create an EvolutionDecision entity in temper-system tenant.
    let system_tenant = TenantId::new("temper-system");
    let ed_id = format!("ED-{}", sim_uuid());
    let ed_params = serde_json::json!({
        "analysis_id": "",
        "decided_by": d_record.header.created_by,
        "rationale": d_record.rationale,
        "verification_summary": "",
        "legacy_record_id": record_id,
        "derived_from": id,
    });
    // Note: EvolutionDecision.CreateDecision has a cross-entity guard on Analysis,
    // but the legacy record may not have a corresponding Analysis entity yet.
    // We dispatch best-effort; failure is non-fatal, but the divergence
    // (record persisted, entity missing) is surfaced via `entity_synced`.
    let entity_synced = match state
        .dispatch_tenant_action(
            &system_tenant,
            "EvolutionDecision",
            &ed_id,
            "CreateDecision",
            ed_params,
            &AgentContext::for_service("evolution-engine"),
        )
        .await
    {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(
                error = %e,
                record_id = %record_id,
                entity_id = %ed_id,
                "failed to create EvolutionDecision entity (cross-entity guard may have blocked); decision record persisted without entity mirror"
            );
            false
        }
    };

    Ok(Json(serde_json::json!({
        "record_id": record_id,
        "entity_id": ed_id,
        "decision": format!("{:?}", d_record.decision),
        "derived_from": id,
        "status": "Open",
        "entity_synced": entity_synced,
    })))
}

/// Minimal chain validation summary.
#[derive(Debug)]
struct ChainValidationSummary {
    is_valid: bool,
    errors: Vec<String>,
    chain_length: usize,
}

async fn validate_chain(state: &ServerState, leaf_id: &str) -> ChainValidationSummary {
    let mut errors = Vec::new();
    let mut chain_length = 0usize;
    let mut current_id = leaf_id.to_string();
    let mut expected_type: Option<RecordType> = None;

    loop {
        chain_length += 1;
        let Some(record_type) = RecordType::from_id_prefix(&current_id) else {
            errors.push(format!("unknown record type prefix in \'{current_id}\'"));
            break;
        };

        if let Some(expected) = expected_type
            && record_type != expected
        {
            errors.push(format!(
                "record \'{current_id}\' is {:?} but expected {:?}",
                record_type, expected
            ));
        }

        expected_type = record_type.expected_parent();

        let derived_from = match state.get_evolution_record(&current_id).await {
            Ok(Some(row)) => row.derived_from,
            Ok(None) => {
                errors.push(format!("record \'{current_id}\' not found"));
                break;
            }
            Err(e) => {
                errors.push(format!("failed to read \'{current_id}\': {e}"));
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
                        "chain root \'{current_id}\' is {:?}, expected Observation",
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
