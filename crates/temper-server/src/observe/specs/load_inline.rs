use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Json;
use temper_evolution::records::{
    AnalysisRecord, ObservationClass, ObservationRecord, RecordHeader, RecordType, SolutionOption,
};
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use crate::authz_helpers::{record_authz_denial, security_context_from_headers};
use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};

use super::load_dir::handle_load_dir;
use super::types::{LoadDirRequest, LoadInlineRequest};

/// POST /api/specs/load-inline -- load specs from inline content.
///
/// Accepts a JSON body with `tenant` and `specs` (map of filename -> content).
/// Cedar-gated: requires `submit_specs` action on `SpecRegistry` resource.
/// Records trajectory for every spec submission (success or denial).
#[instrument(skip_all, fields(otel.name = "POST /api/specs/load-inline"))]
pub(crate) async fn handle_load_inline(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<LoadInlineRequest>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let tenant = body.tenant.clone();

    // Cedar authorization gate.
    let security_ctx = security_context_from_headers(&headers, None, None);
    let entity_names: Vec<String> = body
        .specs
        .keys()
        .filter(|k| k.ends_with(".ioa.toml"))
        .map(|k| k.strip_suffix(".ioa.toml").unwrap_or(k).to_string())
        .collect();

    let mut spec_resource_attrs = std::collections::BTreeMap::new();
    spec_resource_attrs.insert("id".to_string(), serde_json::json!(tenant));
    for (spec_key, spec_content) in &body.specs {
        if spec_key.ends_with(".ioa.toml")
            && let Ok(automaton) = temper_spec::automaton::parse_automaton(spec_content)
        {
            let metadata = automaton.extract_metadata();
            for (k, v) in metadata.to_flat_map() {
                spec_resource_attrs.insert(k, v);
            }
        }
    }

    let authz_result = state.authorize_with_context(
        &security_ctx,
        "submit_specs",
        "SpecRegistry",
        &spec_resource_attrs,
    );
    if let Err(reason) = authz_result {
        // Record denials and use the first decision ID as the primary chain anchor.
        // Record a single denial per authz check. The resource_id must match
        // what Cedar evaluated: SpecRegistry::"<tenant>".
        let pd = record_authz_denial(
            &state,
            &tenant,
            &security_ctx,
            None,
            "submit_specs",
            "SpecRegistry",
            &tenant,
            serde_json::json!({"entity_types": entity_names}),
            &reason,
            None,
            None,
        )
        .await;
        let primary_decision_id = pd.id.clone();
        let decision_ids = vec![pd.id];

        // Create O-Record for the denied spec submission.
        let o_record = ObservationRecord {
            header: RecordHeader::new(RecordType::Observation, "cedar:spec_submission"),
            source: "cedar:spec_submission".to_string(),
            classification: ObservationClass::AuthzDenied,
            evidence_query: format!(
                "Agent '{}' proposed spec for entity types: {:?}",
                security_ctx.principal.id, entity_names,
            ),
            threshold_field: None,
            threshold_value: None,
            observed_value: None,
            context: serde_json::json!({
                "agent_id": security_ctx.principal.id,
                "tenant": tenant,
                "entity_types": entity_names,
                "decision_id": primary_decision_id.clone(),
                "spec_metadata": spec_resource_attrs,
            }),
        };
        let o_id = o_record.header.id.clone();
        // Persist observation to Turso.
        if let Some(turso) = state.turso_opt() {
            let data_json = serde_json::to_string(&o_record).unwrap_or_default();
            let _ = turso
                .insert_evolution_record(
                    &o_record.header.id,
                    "Observation",
                    &format!("{:?}", o_record.header.status),
                    &o_record.header.created_by,
                    o_record.header.derived_from.as_deref(),
                    &data_json,
                )
                .await;
        }

        // Create A-Record with the proposed spec as spec_diff.
        let spec_summary: String = body
            .specs
            .keys()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let a_record = AnalysisRecord {
            header: RecordHeader::new(RecordType::Analysis, "cedar:spec_submission")
                .derived_from(o_id),
            root_cause: format!(
                "Agent proposed new entity types ({}) but lacks Cedar permission.",
                spec_summary,
            ),
            options: vec![SolutionOption {
                description: "Approve spec submission via Observe UI".to_string(),
                spec_diff: serde_json::to_string_pretty(&body.specs).unwrap_or_default(),
                tla_impact: "NEW".to_string(),
                risk: "low".to_string(),
                complexity: "low".to_string(),
            }],
            recommendation: Some(0),
        };
        let a_record_id = a_record.header.id.clone();
        // Persist analysis to Turso.
        if let Some(turso) = state.turso_opt() {
            let data_json = serde_json::to_string(&a_record).unwrap_or_default();
            let _ = turso
                .insert_evolution_record(
                    &a_record.header.id,
                    "Analysis",
                    &format!("{:?}", a_record.header.status),
                    &a_record.header.created_by,
                    a_record.header.derived_from.as_deref(),
                    &data_json,
                )
                .await;
        }

        // Link the PendingDecision to the A-Record for O-A-D chain tracing.
        // Decisions are persisted to Turso; the evolution_record_id link will be
        // available when the decision is read back from Turso.
        if let Some(turso) = state.turso_opt() {
            for decision_id in &decision_ids {
                if let Ok(Some(data_str)) = turso.get_pending_decision(decision_id).await
                    && let Ok(mut pd) =
                        serde_json::from_str::<crate::state::PendingDecision>(&data_str)
                {
                    pd.evolution_record_id = Some(a_record_id.clone());
                    let updated_json = serde_json::to_string(&pd).unwrap_or_default();
                    let status_str = match pd.status {
                        crate::state::DecisionStatus::Pending => "pending",
                        crate::state::DecisionStatus::Approved => "approved",
                        crate::state::DecisionStatus::Denied => "denied",
                        crate::state::DecisionStatus::Expired => "expired",
                    };
                    let _ = turso
                        .upsert_pending_decision(decision_id, &pd.tenant, status_str, &updated_json)
                        .await;
                }
            }
        }

        return Err((
            StatusCode::FORBIDDEN,
            serde_json::json!({
                "error": {
                    "code": "AuthorizationDenied",
                    "message": format!("{reason} Decision {}", primary_decision_id),
                }
            })
            .to_string(),
        ));
    }

    // Record successful spec submission trajectory.
    for entity_name in &entity_names {
        let traj = TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: tenant.clone(),
            entity_type: entity_name.clone(),
            entity_id: String::new(),
            action: "SubmitSpec".to_string(),
            success: true,
            from_status: None,
            to_status: None,
            error: None,
            agent_id: Some(security_ctx.principal.id.clone()),
            session_id: None,
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: Some(TrajectorySource::Entity),
            spec_governed: None,
        };
        if let Err(e) = state.persist_trajectory_entry(&traj).await {
            tracing::error!(error = %e, "failed to persist spec submission trajectory");
        }
    }

    // Write specs to a temp directory
    let tmp_dir = std::env::temp_dir().join(format!("temper-inline-{}", tenant)); // determinism-ok: HTTP handler writes user specs to temp dir for loading
    let _ = std::fs::remove_dir_all(&tmp_dir); // determinism-ok: HTTP handler cleans previous temp dir
    std::fs::create_dir_all(&tmp_dir).map_err(|e| {
        // determinism-ok: HTTP handler creates temp dir for inline specs
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to create temp dir: {e}"),
        )
    })?;

    for (filename, content) in &body.specs {
        let path = tmp_dir.join(filename);
        std::fs::write(&path, content).map_err(|e| {
            // determinism-ok: HTTP handler writes user specs to temp dir
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write {filename}: {e}"),
            )
        })?;
    }

    if let Some(source) = body.cross_invariants_toml.as_deref() {
        std::fs::write(tmp_dir.join("cross-invariants.toml"), source).map_err(|e| {
            // determinism-ok: HTTP handler writes cross-invariants
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write cross-invariants.toml: {e}"),
            )
        })?;
    }

    // Delegate to load-dir logic
    let dir_request = LoadDirRequest {
        tenant,
        specs_dir: tmp_dir.to_string_lossy().to_string(),
    };
    handle_load_dir(State(state), Json(dir_request)).await
}
