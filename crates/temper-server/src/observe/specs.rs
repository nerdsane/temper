//! Spec management endpoints: list, load, and inspect IOA specifications.

use std::convert::Infallible;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use temper_spec::automaton::LintSeverity;
use temper_spec::cross_invariant::{
    CrossInvariantLintSeverity, lint_cross_invariants, parse_cross_invariants,
};
use tokio_stream::wrappers::ReceiverStream;

use temper_runtime::scheduler::sim_now;

use crate::authz_helpers::{record_authz_denial, security_context_from_headers};
use crate::reaction::registry::parse_reactions;
use crate::registry::VerificationStatus;
use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};
use axum::http::HeaderMap;
use temper_evolution::records::{
    AnalysisRecord, ObservationClass, ObservationRecord, RecordHeader, RecordType, SolutionOption,
};

use super::specs_helpers::{
    EntityLintFinding, build_ndjson_response, cross_lint_ndjson_line, lint_loaded_specs,
    lint_ndjson_line, to_pascal_case,
};
use super::{ActionDetail, InvariantDetail, SpecDetail, SpecSummary, StateVarDetail};

/// GET /observe/specs -- list all loaded specs across all tenants.
pub(crate) async fn list_specs(State(state): State<ServerState>) -> Json<Vec<SpecSummary>> {
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
    let mut specs = Vec::new();

    for tenant_id in registry.tenant_ids() {
        for entity_type in registry.entity_types(tenant_id) {
            if let Some(entity_spec) = registry.get_spec(tenant_id, entity_type) {
                let automaton = &entity_spec.automaton;

                // Read verification status
                let (verification_status, levels_passed, levels_total) = match registry
                    .get_verification_status(tenant_id, entity_type)
                {
                    Some(VerificationStatus::Pending) | None => ("pending".to_string(), None, None),
                    Some(VerificationStatus::Running) => ("running".to_string(), None, None),
                    Some(VerificationStatus::Completed(result)) => {
                        let passed = result.levels.iter().filter(|l| l.passed).count();
                        let total = result.levels.len();
                        let status = if result.all_passed {
                            "passed"
                        } else if passed == 0 {
                            "failed"
                        } else {
                            "partial"
                        };
                        (status.to_string(), Some(passed), Some(total))
                    }
                };

                specs.push(SpecSummary {
                    tenant: tenant_id.as_str().to_string(),
                    entity_type: entity_type.to_string(),
                    states: automaton.automaton.states.clone(),
                    actions: automaton.actions.iter().map(|a| a.name.clone()).collect(),
                    initial_state: automaton.automaton.initial.clone(),
                    verification_status,
                    levels_passed,
                    levels_total,
                });
            }
        }
    }

    Json(specs)
}

/// Request body for POST /api/specs/load-dir.
#[derive(Deserialize)]
pub(crate) struct LoadDirRequest {
    /// Tenant name to register specs under.
    tenant: String,
    /// Path to the specs directory containing model.csdl.xml and *.ioa.toml files.
    specs_dir: String,
}

/// Request body for POST /api/specs/load-inline.
#[derive(Deserialize)]
pub(crate) struct LoadInlineRequest {
    /// Tenant name to register specs under.
    tenant: String,
    /// Map of filename -> content. Must include `model.csdl.xml` and at least one `*.ioa.toml`.
    specs: std::collections::BTreeMap<String, String>,
    /// Optional inline `cross-invariants.toml` source.
    #[serde(default)]
    cross_invariants_toml: Option<String>,
}

/// POST /api/specs/load-dir -- hot-load specs from a directory into the running server.///
/// Reads CSDL and IOA files from `specs_dir`, registers them under `tenant`,
/// emits design-time SSE events for each entity, and spawns background
/// verification tasks that stream progress via SSE.
pub(crate) async fn handle_load_dir(
    State(state): State<ServerState>,
    Json(body): Json<LoadDirRequest>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let specs_path = std::path::Path::new(&body.specs_dir);

    if !specs_path.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Specs directory not found: {}", specs_path.display()),
        ));
    }

    // Read CSDL model
    let csdl_path = specs_path.join("model.csdl.xml");
    if !csdl_path.exists() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("CSDL model not found at {}", csdl_path.display()),
        ));
    }

    let csdl_xml = std::fs::read_to_string(&csdl_path).map_err(|e| {
        // determinism-ok: HTTP handler reads spec files
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read CSDL: {e}"),
        )
    })?;
    let csdl = temper_spec::csdl::parse_csdl(&csdl_xml).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Failed to parse CSDL: {e}"),
        )
    })?;

    // Read all *.ioa.toml files
    let mut ioa_sources: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let entries = std::fs::read_dir(specs_path).map_err(|e| {
        // determinism-ok: HTTP handler reads spec directory
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to read specs directory: {e}"),
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read directory entry: {e}"),
            )
        })?;
        let path = entry.path();
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if file_name.ends_with(".ioa.toml") {
            let entity_name = file_name.strip_suffix(".ioa.toml").unwrap_or_default();
            let entity_name = to_pascal_case(entity_name);
            let source = std::fs::read_to_string(&path).map_err(|e| {
                // determinism-ok: HTTP handler reads spec files
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to read {}: {e}", path.display()),
                )
            })?;
            ioa_sources.insert(entity_name, source);
        }
    }

    if ioa_sources.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "No .ioa.toml files found in specs directory".to_string(),
        ));
    }

    // Optional reactions.toml.
    let reactions = {
        let path = specs_path.join("reactions.toml");
        if path.exists() {
            let source = std::fs::read_to_string(&path).map_err(|e| {
                // determinism-ok: HTTP handler reads reactions file
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to read {}: {e}", path.display()),
                )
            })?;
            parse_reactions(&source).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("Failed to parse {}: {e}", path.display()),
                )
            })?
        } else {
            Vec::new()
        }
    };

    // Optional cross-invariants.toml.
    let cross_invariants_toml = {
        let path = specs_path.join("cross-invariants.toml");
        if path.exists() {
            Some(std::fs::read_to_string(&path).map_err(|e| {
                // determinism-ok: HTTP handler reads cross-invariants
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to read {}: {e}", path.display()),
                )
            })?)
        } else {
            None
        }
    };

    let lint_findings = lint_loaded_specs(&csdl, &ioa_sources)?;
    let cross_lint_findings = if let Some(source) = cross_invariants_toml.as_deref() {
        let spec = parse_cross_invariants(source).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("Failed to parse cross-invariants.toml: {e}"),
            )
        })?;
        lint_cross_invariants(&spec)
    } else {
        Vec::new()
    };

    let ioa_lint_errors = lint_findings
        .iter()
        .filter(|f| matches!(f.severity, LintSeverity::Error))
        .count();
    let ioa_lint_warnings = lint_findings
        .iter()
        .filter(|f| matches!(f.severity, LintSeverity::Warning))
        .count();
    let cross_lint_errors = cross_lint_findings
        .iter()
        .filter(|f| matches!(f.severity, CrossInvariantLintSeverity::Error))
        .count();
    let cross_lint_warnings = cross_lint_findings
        .iter()
        .filter(|f| matches!(f.severity, CrossInvariantLintSeverity::Warning))
        .count();
    let lint_errors = ioa_lint_errors + cross_lint_errors;
    let lint_warnings = ioa_lint_warnings + cross_lint_warnings;

    // Register names once so both failure and success paths can report them.
    let entity_names: Vec<String> = ioa_sources.keys().cloned().collect();

    // Abort early on lint errors (no persistence, no registry registration).
    if lint_errors > 0 {
        let mut lines = vec![serde_json::json!({
            "type": "specs_loaded",
            "tenant": &body.tenant,
            "entities": &entity_names,
        })];
        lines.extend(lint_findings.iter().map(lint_ndjson_line));
        lines.extend(cross_lint_findings.iter().map(cross_lint_ndjson_line));
        lines.push(serde_json::json!({
            "type": "summary",
            "tenant": &body.tenant,
            "all_passed": false,
            "lint_errors": lint_errors,
            "lint_warnings": lint_warnings,
            "ioa_lint_errors": ioa_lint_errors,
            "ioa_lint_warnings": ioa_lint_warnings,
            "cross_lint_errors": cross_lint_errors,
            "cross_lint_warnings": cross_lint_warnings,
        }));
        return build_ndjson_response(StatusCode::BAD_REQUEST, lines);
    }

    // Persist loaded specs first when Postgres is configured.
    let csdl_xml_for_db = csdl_xml.clone();
    for (entity_type, ioa_source) in &ioa_sources {
        state
            .upsert_spec_source(&body.tenant, entity_type, ioa_source, &csdl_xml_for_db)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    state
        .upsert_tenant_constraints(&body.tenant, cross_invariants_toml.as_deref())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // Register into shared registry after persistence succeeds.
    let ioa_pairs: Vec<(&str, &str)> = ioa_sources
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    {
        let mut registry = state.registry.write().unwrap(); // ci-ok: infallible lock
        registry
            .try_register_tenant_with_reactions_and_constraints(
                body.tenant.as_str(),
                csdl,
                csdl_xml,
                &ioa_pairs,
                reactions,
                cross_invariants_toml.clone(),
            )
            .map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("Failed to register specs: {e}"),
                )
            })?;
    }
    state.rebuild_reaction_dispatcher();

    if !state.data_dir.as_os_str().is_empty() {
        let registry_path = state.data_dir.join("specs-registry.json");
        let mut specs_registry = std::collections::BTreeMap::<String, String>::new();

        if let Ok(content) = std::fs::read_to_string(&registry_path) {
            // determinism-ok: HTTP handler reads specs registry
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content)
                && let Some(obj) = value.as_object()
            {
                for (tenant, specs_dir) in obj {
                    if let Some(specs_dir) = specs_dir.as_str() {
                        specs_registry.insert(tenant.clone(), specs_dir.to_string());
                    }
                }
            }
        }

        specs_registry.insert(body.tenant.clone(), body.specs_dir.clone());

        if let Ok(encoded) = serde_json::to_string_pretty(&specs_registry) {
            let _ = std::fs::create_dir_all(&state.data_dir); // determinism-ok: HTTP handler creates data dir
            let _ = std::fs::write(registry_path, encoded); // determinism-ok: HTTP handler writes specs registry
        }
    }

    // Stream NDJSON response: verification runs inline and results are streamed per-entity.
    // Any agent calling this endpoint gets verification results without polling.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, Infallible>>(100);
    let tenant = body.tenant.clone();
    let state_for_task = state.clone();
    let lint_warnings_for_stream: Vec<EntityLintFinding> = lint_findings
        .into_iter()
        .filter(|f| matches!(f.severity, LintSeverity::Warning))
        .collect();
    let cross_lint_warnings_for_stream: Vec<_> = cross_lint_findings
        .into_iter()
        .filter(|f| matches!(f.severity, CrossInvariantLintSeverity::Warning))
        .collect();

    tokio::spawn(async move {
        // determinism-ok: HTTP handler streams verification results inline
        let now = sim_now();

        // Emit specs_loaded line
        let _ = tx
            .send(Ok(serde_json::to_string(&serde_json::json!({
                    "type": "specs_loaded",
                    "tenant": &tenant,
                    "entities": &entity_names,
                }))
                .unwrap() // ci-ok: infallible serialization
                    + "\n"))
            .await;

        for finding in &lint_warnings_for_stream {
            let _ = tx
                .send(Ok(serde_json::to_string(&lint_ndjson_line(finding))
                    .unwrap() // ci-ok: infallible serialization
                    + "\n"))
                .await;
        }
        for finding in &cross_lint_warnings_for_stream {
            let _ = tx
                .send(Ok(serde_json::to_string(&cross_lint_ndjson_line(finding))
                    .unwrap() // ci-ok: infallible serialization
                    + "\n"))
                .await;
        }

        let mut entity_results: std::collections::BTreeMap<String, bool> =
            std::collections::BTreeMap::new();

        for entity_name in &entity_names {
            // Emit design-time events for UI (spec_loaded + verify_started)
            let loaded_event = crate::state::DesignTimeEvent {
                kind: "spec_loaded".to_string(),
                entity_type: entity_name.clone(),
                tenant: tenant.clone(),
                summary: format!("Loaded spec for {entity_name}"),
                level: None,
                passed: None,
                timestamp: now.to_rfc3339(),
                step_number: Some(1),
                total_steps: Some(7),
            };
            if let Err(e) = state_for_task.emit_design_time_event(loaded_event).await {
                tracing::error!(tenant = %tenant, entity = %entity_name, error = %e, "failed to emit spec_loaded event");
                let _ = tx
                    .send(Ok(serde_json::to_string(&serde_json::json!({
                            "type": "verification_error",
                            "entity": entity_name,
                            "error": e,
                        }))
                        .unwrap() // ci-ok: infallible serialization
                            + "\n"))
                    .await;
                entity_results.insert(entity_name.clone(), false);
                continue;
            }
            if let Err(e) = state_for_task
                .persist_spec_verification(&tenant, entity_name, "running", None)
                .await
            {
                tracing::error!(tenant = %tenant, entity = %entity_name, error = %e, "failed to persist running verification status");
                let _ = tx
                    .send(Ok(serde_json::to_string(&serde_json::json!({
                            "type": "verification_error",
                            "entity": entity_name,
                            "error": e,
                        }))
                        .unwrap() // ci-ok: infallible serialization
                            + "\n"))
                    .await;
                entity_results.insert(entity_name.clone(), false);
                continue;
            }
            {
                let mut registry = state_for_task.registry.write().unwrap(); // ci-ok: infallible lock
                registry.set_verification_status(
                    &tenant.clone().into(),
                    entity_name,
                    VerificationStatus::Running,
                );
            }

            let started_event = crate::state::DesignTimeEvent {
                kind: "verify_started".to_string(),
                entity_type: entity_name.clone(),
                tenant: tenant.clone(),
                summary: format!("Verification started for {entity_name}"),
                level: None,
                passed: None,
                timestamp: now.to_rfc3339(),
                step_number: Some(2),
                total_steps: Some(7),
            };
            if let Err(e) = state_for_task.emit_design_time_event(started_event).await {
                tracing::error!(tenant = %tenant, entity = %entity_name, error = %e, "failed to emit verify_started event");
                let _ = tx
                    .send(Ok(serde_json::to_string(&serde_json::json!({
                            "type": "verification_error",
                            "entity": entity_name,
                            "error": e,
                        }))
                        .unwrap() // ci-ok: infallible serialization
                            + "\n"))
                    .await;
                entity_results.insert(entity_name.clone(), false);
                continue;
            }

            // Stream verification_started
            let _ = tx
                .send(Ok(serde_json::to_string(&serde_json::json!({
                        "type": "verification_started",
                        "entity": entity_name,
                    }))
                    .unwrap() // ci-ok: infallible serialization
                        + "\n"))
                .await;

            // Run verification (blocking, sequential per entity)
            let ioa_source = ioa_sources[entity_name].clone();
            let result = tokio::task::spawn_blocking(move || {
                // determinism-ok: HTTP handler offloads CPU-intensive verification
                temper_verify::VerificationCascade::from_ioa(&ioa_source)
                    .with_sim_seeds(5)
                    .with_prop_test_cases(100)
                    .run()
            })
            .await;

            let level_labels = [
                "L0_symbolic",
                "L1_model_check",
                "L2_simulation",
                "L3_property_test",
            ];

            match result {
                Ok(cascade_result) => {
                    // Emit verify_level events for UI
                    for (level_idx, level_result) in cascade_result.levels.iter().enumerate() {
                        let level_event = crate::state::DesignTimeEvent {
                            kind: "verify_level".to_string(),
                            entity_type: entity_name.clone(),
                            tenant: tenant.clone(),
                            summary: format!(
                                "{} {} for {}",
                                level_labels.get(level_idx).unwrap_or(&"unknown"),
                                if level_result.passed {
                                    "passed"
                                } else {
                                    "failed"
                                },
                                entity_name
                            ),
                            level: Some(
                                level_labels
                                    .get(level_idx)
                                    .unwrap_or(&"unknown")
                                    .to_string(),
                            ),
                            passed: Some(level_result.passed),
                            timestamp: sim_now().to_rfc3339(),
                            step_number: Some(3 + level_idx as u8),
                            total_steps: Some(7),
                        };
                        if let Err(e) = state_for_task.emit_design_time_event(level_event).await {
                            tracing::error!(tenant = %tenant, entity = %entity_name, error = %e, "failed to emit verify_level event");
                            let _ = tx
                                .send(Ok(serde_json::to_string(&serde_json::json!({
                                        "type": "verification_error",
                                        "entity": entity_name,
                                        "error": e,
                                    }))
                                    .unwrap() // ci-ok: infallible serialization
                                        + "\n"))
                                .await;
                        }
                    }

                    // Build EntityVerificationResult for registry
                    let entity_result = crate::registry::EntityVerificationResult {
                        all_passed: cascade_result.all_passed,
                        levels: cascade_result.levels.iter().map(|l| {
                            let details: Option<Vec<crate::registry::VerificationDetail>> = if !l.passed {
                                let mut dets = Vec::new();
                                if let Some(sim) = &l.simulation {
                                    for v in &sim.liveness_violations {
                                        dets.push(crate::registry::VerificationDetail {
                                            kind: "liveness_violation".into(),
                                            property: v.property.clone(),
                                            description: v.description.clone(),
                                            actor_id: Some(v.actor_id.clone()),
                                        });
                                    }
                                    for v in &sim.violations {
                                        dets.push(crate::registry::VerificationDetail {
                                            kind: "invariant_violation".into(),
                                            property: v.invariant.clone(),
                                            description: format!(
                                                "Actor {} violated invariant at tick {} during action {}",
                                                v.actor_id, v.tick, v.action
                                            ),
                                            actor_id: Some(v.actor_id.clone()),
                                        });
                                    }
                                }
                                if let Some(mc) = &l.verification {
                                    for cx in &mc.counterexamples {
                                        dets.push(crate::registry::VerificationDetail {
                                            kind: "counterexample".into(),
                                            property: cx.property.clone(),
                                            description: format!(
                                                "Counterexample found with {} step trace",
                                                cx.trace.len()
                                            ),
                                            actor_id: None,
                                        });
                                    }
                                }
                                if let Some(pt) = &l.prop_test
                                    && let Some(failure) = &pt.failure {
                                        dets.push(crate::registry::VerificationDetail {
                                            kind: "proptest_failure".into(),
                                            property: failure.invariant.clone(),
                                            description: format!(
                                                "Property test failed after sequence: {}",
                                                failure.action_sequence.join(" -> ")
                                            ),
                                            actor_id: None,
                                        });
                                    }
                                if dets.is_empty() { None } else { Some(dets) }
                            } else {
                                None
                            };

                            crate::registry::EntityLevelSummary {
                                level: format!("{}", l.level),
                                passed: l.passed,
                                summary: l.summary.clone(),
                                details,
                            }
                        }).collect(),
                        verified_at: sim_now().to_rfc3339(),
                    };

                    // Stream verification_result with full level details
                    let levels_json: Vec<serde_json::Value> = entity_result
                        .levels
                        .iter()
                        .map(|l| {
                            let mut obj = serde_json::json!({
                                "level": &l.level,
                                "passed": l.passed,
                                "summary": &l.summary,
                            });
                            if let Some(details) = &l.details {
                                obj["details"] = serde_json::to_value(details).unwrap_or_default();
                            }
                            obj
                        })
                        .collect();

                    let _ = tx
                        .send(Ok(serde_json::to_string(&serde_json::json!({
                                "type": "verification_result",
                                "entity": entity_name,
                                "all_passed": cascade_result.all_passed,
                                "levels": levels_json,
                            }))
                            .unwrap() // ci-ok: infallible serialization
                                + "\n"))
                        .await;

                    entity_results.insert(entity_name.clone(), cascade_result.all_passed);

                    let passed_count = entity_result.levels.iter().filter(|l| l.passed).count();
                    let final_status = if entity_result.all_passed {
                        "passed"
                    } else if passed_count == 0 {
                        "failed"
                    } else {
                        "partial"
                    };
                    if let Err(e) = state_for_task
                        .persist_spec_verification(
                            &tenant,
                            entity_name,
                            final_status,
                            Some(&entity_result),
                        )
                        .await
                    {
                        tracing::error!(tenant = %tenant, entity = %entity_name, error = %e, "failed to persist completed verification status");
                        let _ = tx
                            .send(Ok(serde_json::to_string(&serde_json::json!({
                                    "type": "verification_error",
                                    "entity": entity_name,
                                    "error": e,
                                }))
                                .unwrap() // ci-ok: infallible serialization
                                    + "\n"))
                            .await;
                        continue;
                    }
                    if let Ok(mut reg) = state_for_task.registry.write() {
                        reg.set_verification_status(
                            &tenant.clone().into(),
                            entity_name,
                            VerificationStatus::Completed(entity_result.clone()),
                        );
                    }
                    let done_event = crate::state::DesignTimeEvent {
                        kind: "verify_done".to_string(),
                        entity_type: entity_name.clone(),
                        tenant: tenant.clone(),
                        summary: if cascade_result.all_passed {
                            format!("All verification levels passed for {entity_name}")
                        } else {
                            format!("Verification completed with failures for {entity_name}")
                        },
                        level: None,
                        passed: Some(cascade_result.all_passed),
                        timestamp: sim_now().to_rfc3339(),
                        step_number: Some(7),
                        total_steps: Some(7),
                    };
                    if let Err(e) = state_for_task.emit_design_time_event(done_event).await {
                        tracing::error!(tenant = %tenant, entity = %entity_name, error = %e, "failed to emit verify_done event");
                        let _ = tx
                            .send(Ok(serde_json::to_string(&serde_json::json!({
                                    "type": "verification_error",
                                    "entity": entity_name,
                                    "error": e,
                                }))
                                .unwrap() // ci-ok: infallible serialization
                                    + "\n"))
                            .await;
                    }
                }
                Err(e) => {
                    entity_results.insert(entity_name.clone(), false);
                    let failure_result = crate::registry::EntityVerificationResult {
                        all_passed: false,
                        levels: vec![crate::registry::EntityLevelSummary {
                            level: "VerificationTask".to_string(),
                            passed: false,
                            summary: format!("Verification failed for {entity_name}: {e}"),
                            details: None,
                        }],
                        verified_at: sim_now().to_rfc3339(),
                    };
                    if let Err(persist_err) = state_for_task
                        .persist_spec_verification(
                            &tenant,
                            entity_name,
                            "failed",
                            Some(&failure_result),
                        )
                        .await
                    {
                        tracing::error!(tenant = %tenant, entity = %entity_name, error = %persist_err, "failed to persist failed verification status");
                        let _ = tx
                            .send(Ok(serde_json::to_string(&serde_json::json!({
                                    "type": "verification_error",
                                    "entity": entity_name,
                                    "error": persist_err,
                                }))
                                .unwrap() // ci-ok: infallible serialization
                                    + "\n"))
                            .await;
                        continue;
                    }
                    if let Ok(mut reg) = state_for_task.registry.write() {
                        reg.set_verification_status(
                            &tenant.clone().into(),
                            entity_name,
                            VerificationStatus::Completed(failure_result.clone()),
                        );
                    }
                    let fail_event = crate::state::DesignTimeEvent {
                        kind: "verify_done".to_string(),
                        entity_type: entity_name.clone(),
                        tenant: tenant.clone(),
                        summary: format!("Verification failed for {entity_name}: {e}"),
                        level: None,
                        passed: Some(false),
                        timestamp: sim_now().to_rfc3339(),
                        step_number: Some(7),
                        total_steps: Some(7),
                    };
                    if let Err(event_err) = state_for_task.emit_design_time_event(fail_event).await
                    {
                        tracing::error!(tenant = %tenant, entity = %entity_name, error = %event_err, "failed to emit failed verify_done event");
                    }

                    let _ = tx
                        .send(Ok(serde_json::to_string(&serde_json::json!({
                                "type": "verification_error",
                                "entity": entity_name,
                                "error": format!("{e}"),
                            }))
                            .unwrap() // ci-ok: infallible serialization
                                + "\n"))
                        .await;
                }
            }
        }

        // Stream final summary
        let all_passed = entity_results.values().all(|&p| p);
        let _ = tx
            .send(Ok(serde_json::to_string(&serde_json::json!({
                    "type": "summary",
                    "tenant": &tenant,
                    "all_passed": all_passed,
                    "entities": entity_results,
                }))
                .unwrap() // ci-ok: infallible serialization
                    + "\n"))
            .await;
        // tx drops here, closing the stream
    });

    let stream = ReceiverStream::new(rx);
    let body = axum::body::Body::from_stream(stream);
    Ok(axum::response::Response::builder()
        .header("content-type", "application/x-ndjson")
        .body(body)
        .unwrap()) // ci-ok: Response::builder with valid headers is infallible
}

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
        if spec_key.ends_with(".ioa.toml") && let Ok(automaton) = temper_spec::automaton::parse_automaton(spec_content) {
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

/// GET /observe/specs/{entity} -- full spec detail for a named entity type.
///
/// Searches across all tenants and returns the first match.
pub(crate) async fn get_spec_detail(
    State(state): State<ServerState>,
    Path(entity): Path<String>,
) -> Result<Json<SpecDetail>, StatusCode> {
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock

    for tenant_id in registry.tenant_ids() {
        if let Some(entity_spec) = registry.get_spec(tenant_id, &entity) {
            let automaton = &entity_spec.automaton;
            let detail = SpecDetail {
                entity_type: entity.clone(),
                states: automaton.automaton.states.clone(),
                initial_state: automaton.automaton.initial.clone(),
                actions: automaton
                    .actions
                    .iter()
                    .map(|a| ActionDetail {
                        name: a.name.clone(),
                        kind: a.kind.clone(),
                        from: a.from.clone(),
                        to: a.to.clone(),
                        guards: a.guard.iter().map(|g| format!("{g:?}")).collect(),
                        effects: a.effect.iter().map(|e| format!("{e:?}")).collect(),
                    })
                    .collect(),
                invariants: automaton
                    .invariants
                    .iter()
                    .map(|i| InvariantDetail {
                        name: i.name.clone(),
                        when: i.when.clone(),
                        assertion: i.assert.clone(),
                    })
                    .collect(),
                state_variables: automaton
                    .state
                    .iter()
                    .map(|sv| StateVarDetail {
                        name: sv.name.clone(),
                        var_type: sv.var_type.clone(),
                        initial: sv.initial.clone(),
                    })
                    .collect(),
            };
            return Ok(Json(detail));
        }
    }

    Err(StatusCode::NOT_FOUND)
}
