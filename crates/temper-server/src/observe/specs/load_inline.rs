use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Json;
use temper_evolution::records::{
    AnalysisRecord, ObservationClass, ObservationRecord, RecordHeader, RecordType, SolutionOption,
};
use temper_runtime::scheduler::sim_now;

use crate::authz_helpers::{record_authz_denial, security_context_from_headers};
use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};

use super::load_dir::handle_load_dir;
use super::types::{LoadDirRequest, LoadInlineRequest};

/// POST /api/specs/load-inline -- load specs from inline content.
///
/// Accepts a JSON body with `tenant` and `specs` (map of filename -> content).
/// Cedar-gated: requires `submit_specs` action on `SpecRegistry` resource.
/// Records trajectory for every spec submission (success or denial).
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
    spec_resource_attrs.insert("id".to_string(), serde_json::json!("SpecRegistry"));
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
        let mut decision_ids = Vec::new();
        if entity_names.is_empty() {
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
            );
            decision_ids.push(pd.id);
        } else {
            for entity_name in &entity_names {
                let pd = record_authz_denial(
                    &state,
                    &tenant,
                    &security_ctx,
                    None,
                    "submit_specs",
                    "SpecRegistry",
                    entity_name,
                    serde_json::json!({"entity_type": entity_name}),
                    &reason,
                    None,
                    None,
                );
                decision_ids.push(pd.id);
            }
        }
        let primary_decision_id = decision_ids.first().cloned().unwrap_or_default();

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
        state.record_store.insert_observation(o_record.clone());
        if let Some(ref pg_store) = state.pg_record_store {
            let _ = pg_store.insert_observation(&o_record).await;
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
        state.record_store.insert_analysis(a_record.clone());
        if let Some(ref pg_store) = state.pg_record_store {
            let _ = pg_store.insert_analysis(&a_record).await;
        }

        // Link the PendingDecision to the A-Record for O-A-D chain tracing.
        {
            let mut log = state.pending_decision_log.write().unwrap(); // ci-ok: infallible lock
            for decision_id in decision_ids {
                if let Some(decision) = log.get_mut(&decision_id) {
                    decision.evolution_record_id = Some(a_record_id.clone());
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
        let mut tlog = state.trajectory_log.write().unwrap(); // ci-ok: infallible lock
        tlog.push(traj);
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
