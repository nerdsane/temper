use std::fs; // determinism-ok: authenticated server-local spec staging boundary

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use temper_authz::AuthenticatedRequestContext;
use temper_evolution::records::{
    AnalysisRecord, ObservationClass, ObservationRecord, RecordHeader, RecordType, SolutionOption,
};
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use crate::authz::{
    DenialInput, record_authz_denial, require_authenticated_context, require_tenant_match,
};
use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};

use super::load_dir::{load_specs_from_directory, validate_spec_directory};
use super::types::{LoadDirRequest, LoadInlineRequest};

mod support;
use support::{build_adr_warning_context, resolve_inline_specs_root};

/// POST /api/specs/load-inline -- load specs from inline content.
///
/// Accepts a JSON body with `tenant` and `specs` (map of filename -> content).
/// Cedar-gated: requires `submit_specs` action on `SpecRegistry` resource.
/// Records trajectory for every spec submission (success or denial).
#[instrument(skip_all, fields(otel.name = "POST /api/specs/load-inline"))]
pub(crate) async fn handle_load_inline(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    raw_body: String,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let authenticated = require_authenticated_context(authenticated.as_deref())
        .map_err(|status| (status, "authentication required".to_string()))?;
    let body: LoadInlineRequest = serde_json::from_str(&raw_body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!(
                "Invalid load-inline request body: {e}. Ensure 'specs' is a map of filename strings to content strings."
            ),
        )
    })?;
    let tenant = body.tenant.clone();
    require_tenant_match(authenticated, &tenant)
        .map_err(|status| (status, "credential tenant mismatch".to_string()))?;
    if body
        .cedar_policies
        .as_deref()
        .is_some_and(|policy| !policy.trim().is_empty())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Inline spec submission cannot activate Cedar policy text; use the tenant policy management API and its separate manage_policies authorization"
                .to_string(),
        ));
    }

    let security_ctx = authenticated.security_context();
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
            for (key, value) in metadata.to_flat_map() {
                spec_resource_attrs.insert(key, value);
            }
        }
    }

    if let Err(denial) = state.authorize_with_context(
        security_ctx,
        "submit_specs",
        "SpecRegistry",
        &spec_resource_attrs,
        &tenant,
    ) {
        let reason = denial.to_string();
        let pending_decision = record_authz_denial(
            &state,
            DenialInput {
                tenant: &tenant,
                security_ctx,
                agent_id_override: None,
                action: "submit_specs",
                resource_type: "SpecRegistry",
                resource_id: &tenant,
                resource_attrs: serde_json::json!({"entity_types": entity_names}),
                reason: &reason,
                module_name: None,
                from_status: None,
                intent: authenticated.intent().map(str::to_string),
                session_id: authenticated.session_id().map(str::to_string),
                // Management-plane denial, not a spec-governed dispatch.
                spec_governed: Some(false),
            },
        )
        .await;
        let primary_decision_id = pending_decision.id.clone();
        let decision_ids = vec![pending_decision.id];

        let observation = ObservationRecord {
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
        let observation_id = observation.header.id.clone();
        let data_json = serde_json::to_string(&observation).unwrap_or_default();
        let _ = state
            .insert_evolution_record(crate::storage::EvolutionRecordWrite {
                tenant: &tenant,
                id: &observation.header.id,
                record_type: "Observation",
                status: &format!("{:?}", observation.header.status),
                created_by: &observation.header.created_by,
                derived_from: observation.header.derived_from.as_deref(),
                data_json: &data_json,
            })
            .await;

        let spec_summary = body
            .specs
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let analysis = AnalysisRecord {
            header: RecordHeader::new(RecordType::Analysis, "cedar:spec_submission")
                .derived_from(observation_id),
            root_cause: format!(
                "Agent proposed new entity types ({spec_summary}) but lacks Cedar permission."
            ),
            options: vec![SolutionOption {
                description: "Approve spec submission via Observe UI".to_string(),
                spec_diff: serde_json::to_string_pretty(&body.specs).unwrap_or_default(),
                tla_impact: "NEW".to_string(),
                risk: temper_evolution::SolutionRisk::Low,
                complexity: temper_evolution::Complexity::Low,
            }],
            recommendation: Some(0),
        };
        let analysis_id = analysis.header.id.clone();
        let data_json = serde_json::to_string(&analysis).unwrap_or_default();
        let _ = state
            .insert_evolution_record(crate::storage::EvolutionRecordWrite {
                tenant: &tenant,
                id: &analysis.header.id,
                record_type: "Analysis",
                status: &format!("{:?}", analysis.header.status),
                created_by: &analysis.header.created_by,
                derived_from: analysis.header.derived_from.as_deref(),
                data_json: &data_json,
            })
            .await;

        if let Some(store) = state.metadata_store_for_tenant(&tenant).await {
            for decision_id in &decision_ids {
                if let Ok(Some(data_str)) = store.get_pending_decision(&tenant, decision_id).await
                    && let Ok(mut decision) =
                        serde_json::from_str::<crate::state::PendingDecision>(&data_str)
                {
                    decision.evolution_record_id = Some(analysis_id.clone());
                    let _ = state.persist_pending_decision(&decision).await;
                }
            }
        }

        return Err((
            StatusCode::FORBIDDEN,
            serde_json::json!({
                "error": {
                    "code": "AuthorizationDenied",
                    "message": format!("{reason} Decision {primary_decision_id}"),
                }
            })
            .to_string(),
        ));
    }

    let temp_dir = tempfile::Builder::new()
        .prefix("temper-inline-")
        .tempdir()
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create isolated spec staging directory: {error}"),
            )
        })?;
    let specs_root = resolve_inline_specs_root(temp_dir.path(), &body.specs)?;

    for (filename, content) in &body.specs {
        let path = temp_dir.path().join(filename);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to create parent directory for {filename}: {error}"),
                )
            })?;
        }
        fs::write(&path, content).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write {filename}: {error}"),
            )
        })?;
    }

    if let Some(source) = body.cross_invariants_toml.as_deref() {
        fs::write(specs_root.join("cross-invariants.toml"), source).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to write cross-invariants.toml: {error}"),
            )
        })?;
    }

    let dir_request = LoadDirRequest {
        tenant: tenant.clone(),
        specs_dir: specs_root.to_string_lossy().to_string(),
        merge: true,
    };
    let directory = validate_spec_directory(&dir_request.specs_dir)?;
    let result = load_specs_from_directory(state.clone(), dir_request, directory).await;
    if let Err(error) = temp_dir.close() {
        tracing::warn!(error = %error, "failed to remove isolated inline-spec staging directory");
    }

    if result.is_ok() {
        let warning_context = build_adr_warning_context(&state, &body, &tenant).await;
        for entity_name in &entity_names {
            let trajectory = TrajectoryEntry {
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
                // Caller-declared session, carried beside the credential rather
                // than inside it: `context_attrs` is the Cedar context.
                session_id: authenticated.session_id().map(str::to_string),
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
                source: Some(TrajectorySource::Entity),
                // Not a governed dispatch: the kernel never ran an action here,
                // and the row's session and entity type are caller-chosen. Left
                // ungoverned it is walked as an ActorExecution, so a caller could
                // post `SubmitSpec` into another run's session and flip that
                // run's conformance verdict on an undeclared action.
                spec_governed: Some(false),
                agent_type: security_ctx.principal.agent_type.clone(),
                request_body: warning_context.clone(),
                intent: None,
                matched_policy_ids: None,
                capture_seq: None,
            };
            if !state.enqueue_trajectory_entry(trajectory) {
                tracing::warn!("failed to enqueue spec submission trajectory");
            }
        }
    }

    result
}
