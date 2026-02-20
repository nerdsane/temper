//! Observe API routes for developer tooling.
//!
//! These endpoints expose internal Temper state for the observability frontend.
//! They are only available when the `observe` feature is enabled.

use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::sim_now;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};

use temper_evolution::{
    Decision, DecisionRecord, RecordHeader, RecordStatus, RecordType, validate_chain,
};

use crate::dispatch::extract_tenant;
use crate::entity_actor::{EntityEvent, EntityMsg, EntityResponse};
use crate::registry::VerificationStatus;
use crate::sentinel;
use crate::state::{ServerState, TrajectoryEntry};

/// Summary of a loaded spec.
#[derive(Serialize, Deserialize)]
pub struct SpecSummary {
    /// Tenant that owns this spec.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// Valid status states.
    pub states: Vec<String>,
    /// Action names.
    pub actions: Vec<String>,
    /// Initial state.
    pub initial_state: String,
    /// Verification status: "pending", "running", "passed", "failed", "partial".
    pub verification_status: String,
    /// Number of verification levels that passed (if completed).
    pub levels_passed: Option<usize>,
    /// Total number of verification levels (if completed).
    pub levels_total: Option<usize>,
}

/// Full spec detail.
#[derive(Serialize, Deserialize)]
pub struct SpecDetail {
    /// Entity type name.
    pub entity_type: String,
    /// Valid status states.
    pub states: Vec<String>,
    /// Initial state.
    pub initial_state: String,
    /// Action details.
    pub actions: Vec<ActionDetail>,
    /// Invariant details.
    pub invariants: Vec<InvariantDetail>,
    /// State variable declarations.
    pub state_variables: Vec<StateVarDetail>,
}

/// Detail of a single action.
#[derive(Serialize, Deserialize)]
pub struct ActionDetail {
    /// Action name.
    pub name: String,
    /// Action kind (input/output/internal).
    pub kind: String,
    /// States from which this action can fire.
    pub from: Vec<String>,
    /// Target state after firing.
    pub to: Option<String>,
    /// Guard conditions (Debug representation).
    pub guards: Vec<String>,
    /// Effects (Debug representation).
    pub effects: Vec<String>,
}

/// Detail of a single invariant.
#[derive(Serialize, Deserialize)]
pub struct InvariantDetail {
    /// Invariant name.
    pub name: String,
    /// Trigger states (empty = always checked).
    pub when: Vec<String>,
    /// Assertion expression.
    pub assertion: String,
}

/// Detail of a state variable.
#[derive(Serialize, Deserialize)]
pub struct StateVarDetail {
    /// Variable name.
    pub name: String,
    /// Variable type.
    pub var_type: String,
    /// Initial value.
    pub initial: String,
}

/// Entity instance summary.
#[derive(Serialize, Deserialize)]
pub struct EntityInstanceSummary {
    /// Entity type.
    pub entity_type: String,
    /// Entity ID.
    pub entity_id: String,
    /// Actor liveness status (e.g. "active", "stopped").
    pub actor_status: String,
    /// Current state of the entity (e.g. "Open", "InProgress").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_state: Option<String>,
    /// ISO 8601 timestamp of the last state change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<String>,
}

/// Query parameters for the simulation endpoint.
#[derive(Deserialize)]
pub struct SimQueryParams {
    /// PRNG seed (default: 42).
    pub seed: Option<u64>,
    /// Max simulation ticks (default: 200).
    pub ticks: Option<u64>,
}

/// Query parameters for the SSE event stream.
#[derive(Deserialize)]
pub struct EventStreamParams {
    /// Filter by entity type.
    pub entity_type: Option<String>,
    /// Filter by entity ID.
    pub entity_id: Option<String>,
}

/// Build the observe router (mounted at /observe).
pub fn build_observe_router() -> Router<ServerState> {
    Router::new()
        .route("/specs", get(list_specs))
        .route("/specs/load-dir", post(handle_load_dir))
        .route("/specs/load-inline", post(handle_load_inline))
        .route("/specs/{entity}", get(get_spec_detail))
        .route("/entities", get(list_entities))
        .route("/verify/{entity}", post(run_verification))
        .route("/simulation/{entity}", get(run_simulation))
        .route(
            "/entities/{entity_type}/{entity_id}/history",
            get(get_entity_history),
        )
        .route("/events/stream", get(handle_event_stream))
        .route("/verification-status", get(handle_verification_status))
        .route("/design-time/stream", get(handle_design_time_stream))
        .route("/workflows", get(handle_workflows))
        .route("/health", get(handle_health))
        .route("/metrics", get(handle_metrics))
        .route("/trajectories", get(handle_trajectories))
        .route("/trajectories/unmet", post(handle_unmet_intent))
        .route("/sentinel/check", post(handle_sentinel_check))
        .route("/evolution/records", get(list_evolution_records))
        .route("/evolution/records/{id}", get(get_evolution_record))
        .route("/evolution/records/{id}/decide", post(handle_decide))
        .route("/evolution/insights", get(list_evolution_insights))
        .route("/skills/builder", get(serve_builder_skill))
        .route("/skills/user", get(serve_user_skill))
}

/// GET /observe/skills/builder -- serve the Builder Agent skill file with dynamic base URL.
async fn serve_builder_skill(
    headers: HeaderMap,
) -> (
    StatusCode,
    [(axum::http::header::HeaderName, &'static str); 1],
    String,
) {
    let base_url = extract_base_url(&headers);
    let content = include_str!("../../../.claude/skills/temper.md")
        .replace("http://localhost:3333", &base_url)
        .replace("http://127.0.0.1:3333", &base_url);
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/markdown; charset=utf-8",
        )],
        content,
    )
}

/// GET /observe/skills/user -- serve the User Agent skill file with dynamic base URL.
async fn serve_user_skill(
    headers: HeaderMap,
) -> (
    StatusCode,
    [(axum::http::header::HeaderName, &'static str); 1],
    String,
) {
    let base_url = extract_base_url(&headers);
    let content = include_str!("../../../.claude/skills/temper-user.md")
        .replace("http://localhost:3333", &base_url)
        .replace("http://127.0.0.1:3333", &base_url);
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/markdown; charset=utf-8",
        )],
        content,
    )
}

/// Extract base URL from request headers (uses X-Forwarded-Host/Proto for proxies like ngrok).
fn extract_base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:3333");
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(if host.contains("ngrok") || host.contains("ts.net") {
            "https"
        } else {
            "http"
        });
    format!("{proto}://{host}")
}

/// GET /observe/specs -- list all loaded specs across all tenants.
async fn list_specs(State(state): State<ServerState>) -> Json<Vec<SpecSummary>> {
    let registry = state.registry.read().unwrap();
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

/// Request body for POST /observe/specs/load-dir.
#[derive(Deserialize)]
struct LoadDirRequest {
    /// Tenant name to register specs under.
    tenant: String,
    /// Path to the specs directory containing model.csdl.xml and *.ioa.toml files.
    specs_dir: String,
}

/// Request body for POST /observe/specs/load-inline.
#[derive(Deserialize)]
struct LoadInlineRequest {
    /// Tenant name to register specs under.
    tenant: String,
    /// Map of filename → content. Must include `model.csdl.xml` and at least one `*.ioa.toml`.
    specs: std::collections::BTreeMap<String, String>,
}

/// Convert a string to PascalCase (e.g. "my_entity" -> "MyEntity").
fn to_pascal_case(s: &str) -> String {
    s.split(['_', '-'])
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let upper: String = first.to_uppercase().collect();
                    format!("{}{}", upper, chars.collect::<String>())
                }
                None => String::new(),
            }
        })
        .collect()
}

/// POST /observe/specs/load-dir -- hot-load specs from a directory into the running server.
///
/// Reads CSDL and IOA files from `specs_dir`, registers them under `tenant`,
/// emits design-time SSE events for each entity, and spawns background
/// verification tasks that stream progress via SSE.
async fn handle_load_dir(
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
        // determinism-ok: HTTP handler reads user-provided spec files
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
        // determinism-ok: HTTP handler reads user-provided spec files
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
                // determinism-ok: HTTP handler reads user-provided spec files
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

    // Persist loaded specs first when Postgres is configured.
    let csdl_xml_for_db = csdl_xml.clone();
    for (entity_type, ioa_source) in &ioa_sources {
        state
            .upsert_spec_source(&body.tenant, entity_type, ioa_source, &csdl_xml_for_db)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }

    // Register into shared registry after persistence succeeds.
    let entity_names: Vec<String> = ioa_sources.keys().cloned().collect();
    let ioa_pairs: Vec<(&str, &str)> = ioa_sources
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    {
        let mut registry = state.registry.write().unwrap();
        registry.register_tenant(body.tenant.as_str(), csdl, csdl_xml, &ioa_pairs);
    }

    // Stream NDJSON response: verification runs inline and results are streamed per-entity.
    // Any agent calling this endpoint gets verification results without polling.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, Infallible>>(100);
    let tenant = body.tenant.clone();
    let state_for_task = state.clone();

    tokio::spawn(async move {
        // determinism-ok: HTTP handler streams verification results inline
        let now = sim_now();

        // Emit specs_loaded line
        let _ = tx
            .send(Ok(
                serde_json::to_string(&serde_json::json!({
                    "type": "specs_loaded",
                    "tenant": &tenant,
                    "entities": &entity_names,
                }))
                .unwrap()
                    + "\n", // ci-ok: serde_json::to_string on valid JSON is infallible
            ))
            .await;

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
                    .send(Ok(
                        serde_json::to_string(&serde_json::json!({
                            "type": "verification_error",
                            "entity": entity_name,
                            "error": e,
                        }))
                        .unwrap()
                            + "\n",
                    ))
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
                    .send(Ok(
                        serde_json::to_string(&serde_json::json!({
                            "type": "verification_error",
                            "entity": entity_name,
                            "error": e,
                        }))
                        .unwrap()
                            + "\n",
                    ))
                    .await;
                entity_results.insert(entity_name.clone(), false);
                continue;
            }
            {
                let mut registry = state_for_task.registry.write().unwrap();
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
                    .send(Ok(
                        serde_json::to_string(&serde_json::json!({
                            "type": "verification_error",
                            "entity": entity_name,
                            "error": e,
                        }))
                        .unwrap()
                            + "\n",
                    ))
                    .await;
                entity_results.insert(entity_name.clone(), false);
                continue;
            }

            // Stream verification_started
            let _ = tx
                .send(Ok(
                    serde_json::to_string(&serde_json::json!({
                        "type": "verification_started",
                        "entity": entity_name,
                    }))
                    .unwrap()
                        + "\n", // ci-ok: serde_json::to_string on valid JSON is infallible
                ))
                .await;

            // Run verification (blocking, sequential per entity)
            let ioa_source = ioa_sources[entity_name].clone();
            let result = tokio::task::spawn_blocking(move || {
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
                                .send(Ok(
                                    serde_json::to_string(&serde_json::json!({
                                        "type": "verification_error",
                                        "entity": entity_name,
                                        "error": e,
                                    }))
                                    .unwrap()
                                        + "\n",
                                ))
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
                                if let Some(pt) = &l.prop_test {
                                    if let Some(failure) = &pt.failure {
                                        dets.push(crate::registry::VerificationDetail {
                                            kind: "proptest_failure".into(),
                                            property: failure.invariant.clone(),
                                            description: format!(
                                                "Property test failed after sequence: {}",
                                                failure.action_sequence.join(" → ")
                                            ),
                                            actor_id: None,
                                        });
                                    }
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
                        .send(Ok(
                            serde_json::to_string(&serde_json::json!({
                                "type": "verification_result",
                                "entity": entity_name,
                                "all_passed": cascade_result.all_passed,
                                "levels": levels_json,
                            }))
                            .unwrap()
                                + "\n", // ci-ok: serde_json::to_string on valid JSON is infallible
                        ))
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
                            .send(Ok(
                                serde_json::to_string(&serde_json::json!({
                                    "type": "verification_error",
                                    "entity": entity_name,
                                    "error": e,
                                }))
                                .unwrap()
                                    + "\n",
                            ))
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
                            .send(Ok(
                                serde_json::to_string(&serde_json::json!({
                                    "type": "verification_error",
                                    "entity": entity_name,
                                    "error": e,
                                }))
                                .unwrap()
                                    + "\n",
                            ))
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
                            .send(Ok(
                                serde_json::to_string(&serde_json::json!({
                                    "type": "verification_error",
                                    "entity": entity_name,
                                    "error": persist_err,
                                }))
                                .unwrap()
                                    + "\n",
                            ))
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
                    if let Err(event_err) = state_for_task.emit_design_time_event(fail_event).await {
                        tracing::error!(tenant = %tenant, entity = %entity_name, error = %event_err, "failed to emit failed verify_done event");
                    }

                    let _ = tx
                        .send(Ok(
                            serde_json::to_string(&serde_json::json!({
                                "type": "verification_error",
                                "entity": entity_name,
                                "error": format!("{e}"),
                            }))
                            .unwrap()
                                + "\n", // ci-ok: serde_json::to_string on valid JSON is infallible
                        ))
                        .await;
                }
            }
        }

        // Stream final summary
        let all_passed = entity_results.values().all(|&p| p);
        let _ = tx
            .send(Ok(
                serde_json::to_string(&serde_json::json!({
                    "type": "summary",
                    "tenant": &tenant,
                    "all_passed": all_passed,
                    "entities": entity_results,
                }))
                .unwrap()
                    + "\n", // ci-ok: serde_json::to_string on valid JSON is infallible
            ))
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

/// POST /observe/specs/load-inline -- load specs from inline content.
///
/// Accepts a JSON body with `tenant` and `specs` (map of filename → content).
/// Writes them to a temp directory and delegates to the same logic as load-dir.
async fn handle_load_inline(
    State(state): State<ServerState>,
    Json(body): Json<LoadInlineRequest>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    // Write specs to a temp directory
    let tmp_dir = std::env::temp_dir().join(format!("temper-inline-{}", body.tenant)); // determinism-ok: HTTP handler writes user specs to temp dir for loading
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

    // Delegate to load-dir logic
    let dir_request = LoadDirRequest {
        tenant: body.tenant,
        specs_dir: tmp_dir.to_string_lossy().to_string(),
    };
    handle_load_dir(State(state), Json(dir_request)).await
}

/// GET /observe/specs/{entity} -- full spec detail for a named entity type.
///
/// Searches across all tenants and returns the first match.
async fn get_spec_detail(
    State(state): State<ServerState>,
    Path(entity): Path<String>,
) -> Result<Json<SpecDetail>, StatusCode> {
    let registry = state.registry.read().unwrap();

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

/// GET /observe/entities -- list active entity instances from the actor registry.
///
/// Returns deduplicated entities with their current state, sorted newest first.
async fn list_entities(State(state): State<ServerState>) -> Json<Vec<EntityInstanceSummary>> {
    let registry = state.actor_registry.read().unwrap(); // ci-ok: infallible lock
    let cache = state.entity_state_cache.read().unwrap(); // ci-ok: infallible lock
    let mut entities: Vec<EntityInstanceSummary> = registry
        .keys()
        .map(|key| {
            // Actor keys are formatted as "{tenant}:{entity_type}:{entity_id}"
            let parts: Vec<&str> = key.splitn(3, ':').collect();
            let (current_state, last_updated) = cache
                .get(key.as_str())
                .map(|(s, t)| (Some(s.clone()), Some(t.to_rfc3339())))
                .unwrap_or((None, None));
            EntityInstanceSummary {
                entity_type: parts.get(1).unwrap_or(&"unknown").to_string(),
                entity_id: parts.get(2).unwrap_or(&"unknown").to_string(),
                actor_status: "active".to_string(),
                current_state,
                last_updated,
            }
        })
        .collect();
    // Sort newest first (by last_updated descending, entities without timestamps go last)
    entities.sort_by(|a, b| b.last_updated.cmp(&a.last_updated));
    Json(entities)
}

/// POST /observe/verify/{entity} -- run verification cascade on a spec.
///
/// Runs all levels (L0 SMT, L1 Model Check, L2 DST, L3 PropTest) and returns results.
/// Emits per-level `DesignTimeEvent`s via SSE so the UI can show streaming progress.
async fn run_verification(
    State(state): State<ServerState>,
    Path(entity): Path<String>,
) -> Result<Json<temper_verify::CascadeResult>, StatusCode> {
    let lookup = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
        let mut found = None;
        for tenant_id in registry.tenant_ids() {
            if let Some(entity_spec) = registry.get_spec(tenant_id, &entity) {
                found = Some((tenant_id.clone(), entity_spec.ioa_source.clone()));
                break;
            }
        }
        found
    };

    let Some((tenant_id, ioa_source)) = lookup else {
        return Err(StatusCode::NOT_FOUND);
    };
    let tenant = tenant_id.as_str().to_string();

    // Persist first, then update in-memory registry.
    state
        .persist_spec_verification(&tenant, &entity, "running", None)
        .await
        .map_err(|e| {
            tracing::error!(tenant = %tenant, entity = %entity, error = %e, "failed to persist running verification status");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if let Ok(mut reg) = state.registry.write() {
        reg.set_verification_status(&tenant_id, &entity, VerificationStatus::Running);
    }

    // Emit verify_started event
    let now = sim_now();
    let started_event = crate::state::DesignTimeEvent {
        kind: "verify_started".to_string(),
        entity_type: entity.clone(),
        tenant: tenant.clone(),
        summary: format!("Verification started for {entity}"),
        level: None,
        passed: None,
        timestamp: now.to_rfc3339(),
        step_number: Some(2),
        total_steps: Some(7),
    };
    state.emit_design_time_event(started_event).await.map_err(|e| {
        tracing::error!(tenant = %tenant, entity = %entity, error = %e, "failed to emit verify_started event");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Run the cascade in a blocking task since verification is CPU-intensive.
    let result = tokio::task::spawn_blocking(move || {
        temper_verify::VerificationCascade::from_ioa(&ioa_source)
            .with_sim_seeds(5)
            .with_prop_test_cases(100)
            .run()
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Emit per-level events so the UI stepper can update incrementally
    for (i, level_result) in result.levels.iter().enumerate() {
        let level_name = level_result.level.to_string();
        let event = crate::state::DesignTimeEvent {
            kind: "verify_level".to_string(),
            entity_type: entity.clone(),
            tenant: tenant.clone(),
            summary: format!("{level_name}: {}", level_result.summary),
            level: Some(level_name),
            passed: Some(level_result.passed),
            timestamp: sim_now().to_rfc3339(),
            step_number: Some(3 + i as u8),
            total_steps: Some(7),
        };
        state.emit_design_time_event(event).await.map_err(|e| {
            tracing::error!(tenant = %tenant, entity = %entity, error = %e, "failed to emit verify_level event");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }

    let entity_result = crate::registry::EntityVerificationResult {
        all_passed: result.all_passed,
        levels: result
            .levels
            .iter()
            .map(|l| crate::registry::EntityLevelSummary {
                level: l.level.to_string(),
                passed: l.passed,
                summary: l.summary.clone(),
                details: None,
            })
            .collect(),
        verified_at: sim_now().to_rfc3339(),
    };
    let passed_count = entity_result.levels.iter().filter(|l| l.passed).count();
    let final_status = if entity_result.all_passed {
        "passed"
    } else if passed_count == 0 {
        "failed"
    } else {
        "partial"
    };
    state
        .persist_spec_verification(&tenant, &entity, final_status, Some(&entity_result))
        .await
        .map_err(|e| {
            tracing::error!(tenant = %tenant, entity = %entity, error = %e, "failed to persist final verification status");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if let Ok(mut reg) = state.registry.write() {
        reg.set_verification_status(
            &tenant_id,
            &entity,
            VerificationStatus::Completed(entity_result.clone()),
        );
    }

    // Emit verify_done event
    let done_event = crate::state::DesignTimeEvent {
        kind: "verify_done".to_string(),
        entity_type: entity.clone(),
        tenant: tenant.clone(),
        summary: if result.all_passed {
            format!("All levels passed for {entity}")
        } else {
            format!("Verification failed for {entity}")
        },
        level: None,
        passed: Some(result.all_passed),
        timestamp: sim_now().to_rfc3339(),
        step_number: Some(7),
        total_steps: Some(7),
    };
    state.emit_design_time_event(done_event).await.map_err(|e| {
        tracing::error!(tenant = %tenant, entity = %entity, error = %e, "failed to emit verify_done event");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(result))
}

/// GET /observe/simulation/{entity}?seed=N&ticks=M -- run deterministic simulation.
///
/// Runs a single-seed simulation with light fault injection and returns the result.
async fn run_simulation(
    State(state): State<ServerState>,
    Path(entity): Path<String>,
    Query(params): Query<SimQueryParams>,
) -> Result<Json<temper_verify::SimulationResult>, StatusCode> {
    let ioa_source = {
        let registry = state.registry.read().unwrap();
        let mut found = None;
        for tenant_id in registry.tenant_ids() {
            if let Some(entity_spec) = registry.get_spec(tenant_id, &entity) {
                found = Some(entity_spec.ioa_source.clone());
                break;
            }
        }
        found
    };

    let Some(ioa_source) = ioa_source else {
        return Err(StatusCode::NOT_FOUND);
    };

    let seed = params.seed.unwrap_or(42);
    let ticks = params.ticks.unwrap_or(200);

    let result = tokio::task::spawn_blocking(move || {
        let config = temper_verify::SimConfig {
            seed,
            max_ticks: ticks,
            num_actors: 3,
            max_actions_per_actor: 20,
            max_counter: 2,
            faults: temper_runtime::scheduler::FaultConfig::light(),
        };
        temper_verify::run_simulation_from_ioa(&ioa_source, &config)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(result))
}

/// GET /observe/entities/{entity_type}/{entity_id}/history -- entity event history.
///
/// Returns the full event log for an entity. Checks two sources in order:
/// 1. In-memory actor state (if the actor is currently loaded).
/// 2. Postgres event store (if configured, for inactive entities).
async fn get_entity_history(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path((entity_type, entity_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let tenant = extract_tenant(&headers, &state);

    // Path 1: If the actor is loaded, read events from in-memory state.
    let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
    let actor_ref = {
        let registry = state
            .actor_registry
            .read()
            .unwrap_or_else(|e| e.into_inner());
        registry.get(&actor_key).cloned()
    };

    if let Some(actor_ref) = actor_ref {
        if let Ok(response) = actor_ref
            .ask::<EntityResponse>(EntityMsg::GetState, Duration::from_secs(5))
            .await
        {
            let mut json =
                format_history_response(&entity_type, &entity_id, &response.state.events);
            // Include entity properties from in-memory state.
            if let Some(obj) = json.as_object_mut() {
                obj.insert(
                    "current_state".to_string(),
                    serde_json::json!(response.state.status),
                );
                obj.insert("fields".to_string(), response.state.fields.clone());
                obj.insert(
                    "counters".to_string(),
                    serde_json::json!(response.state.counters),
                );
                obj.insert(
                    "booleans".to_string(),
                    serde_json::json!(response.state.booleans),
                );
                obj.insert("lists".to_string(), serde_json::json!(response.state.lists));
            }
            return Json(json);
        }
    }

    // Path 2: Query Postgres event store directly (for inactive entities).
    if let Some(ref store) = state.event_store {
        let persistence_id = format!("{entity_type}:{entity_id}");
        if let Ok(envelopes) = store.read_events(&persistence_id, 0).await {
            let events: Vec<serde_json::Value> = envelopes
                .iter()
                .filter_map(|env| serde_json::from_value::<EntityEvent>(env.payload.clone()).ok())
                .enumerate()
                .map(|(i, event)| {
                    serde_json::json!({
                        "sequence": i + 1,
                        "action": event.action,
                        "from_state": event.from_status,
                        "to_state": event.to_status,
                        "timestamp": event.timestamp,
                        "params": event.params,
                    })
                })
                .collect();

            return Json(serde_json::json!({
                "entity_type": entity_type,
                "entity_id": entity_id,
                "events": events,
            }));
        }
    }

    // No data sources available.
    Json(serde_json::json!({
        "entity_type": entity_type,
        "entity_id": entity_id,
        "events": [],
    }))
}

/// Format entity events into the history API response shape.
fn format_history_response(
    entity_type: &str,
    entity_id: &str,
    events: &[EntityEvent],
) -> serde_json::Value {
    let formatted: Vec<serde_json::Value> = events
        .iter()
        .enumerate()
        .map(|(i, e)| {
            serde_json::json!({
                "sequence": i + 1,
                "action": e.action,
                "from_state": e.from_status,
                "to_state": e.to_status,
                "timestamp": e.timestamp,
                "params": e.params,
            })
        })
        .collect();

    serde_json::json!({
        "entity_type": entity_type,
        "entity_id": entity_id,
        "events": formatted,
    })
}

// ---------------------------------------------------------------------------
// Phase 2: SSE event stream
// ---------------------------------------------------------------------------

/// GET /observe/events/stream -- Server-Sent Events stream of entity transitions.
///
/// Subscribes to the broadcast channel and streams every `EntityStateChange`
/// as a JSON SSE event. Supports optional `?entity_type=X&entity_id=Y` filters.
async fn handle_event_stream(
    State(state): State<ServerState>,
    Query(params): Query<EventStreamParams>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_tx.subscribe();
    let filter_type = params.entity_type;
    let filter_id = params.entity_id;

    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        match result {
            Ok(change) => {
                // Apply filters.
                if let Some(ref ft) = filter_type {
                    if change.entity_type != *ft {
                        return None;
                    }
                }
                if let Some(ref fi) = filter_id {
                    if change.entity_id != *fi {
                        return None;
                    }
                }
                let data = serde_json::to_string(&change).unwrap_or_default();
                Some(Ok(Event::default().event("state_change").data(data)))
            }
            // Lagged receiver: skip missed events and continue.
            Err(_) => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------------
// Design-time observation: verification status & SSE stream
// ---------------------------------------------------------------------------

/// Response shape for GET /observe/verification-status.
#[derive(Serialize)]
struct AllVerificationStatus {
    /// Aggregate counts.
    pending: usize,
    running: usize,
    passed: usize,
    failed: usize,
    partial: usize,
    /// Per-entity details.
    entities: Vec<EntityVerificationStatusResponse>,
}

/// Per-entity verification status in the response.
#[derive(Serialize)]
struct EntityVerificationStatusResponse {
    tenant: String,
    entity_type: String,
    status: String,
    levels: Option<Vec<serde_json::Value>>,
    verified_at: Option<String>,
}

/// GET /observe/verification-status -- all entity verification statuses.
async fn handle_verification_status(
    State(state): State<ServerState>,
) -> Json<AllVerificationStatus> {
    let registry = state.registry.read().unwrap();
    let mut pending = 0usize;
    let mut running = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut partial = 0usize;
    let mut entities = Vec::new();

    for tenant_id in registry.tenant_ids() {
        if let Some(statuses) = registry.verification_statuses(tenant_id) {
            for (entity_type, status) in statuses {
                let (status_str, levels, verified_at) = match status {
                    VerificationStatus::Pending => {
                        pending += 1;
                        ("pending".to_string(), None, None)
                    }
                    VerificationStatus::Running => {
                        running += 1;
                        ("running".to_string(), None, None)
                    }
                    VerificationStatus::Completed(result) => {
                        let passed_count = result.levels.iter().filter(|l| l.passed).count();
                        let s = if result.all_passed {
                            passed += 1;
                            "passed"
                        } else if passed_count == 0 {
                            failed += 1;
                            "failed"
                        } else {
                            partial += 1;
                            "partial"
                        };
                        let lvls: Vec<serde_json::Value> = result
                            .levels
                            .iter()
                            .map(|l| {
                                let mut obj = serde_json::json!({
                                    "level": l.level,
                                    "passed": l.passed,
                                    "summary": l.summary,
                                });
                                if let Some(details) = &l.details {
                                    obj["details"] =
                                        serde_json::to_value(details).unwrap_or_default();
                                }
                                obj
                            })
                            .collect();
                        (s.to_string(), Some(lvls), Some(result.verified_at.clone()))
                    }
                };

                entities.push(EntityVerificationStatusResponse {
                    tenant: tenant_id.as_str().to_string(),
                    entity_type: entity_type.clone(),
                    status: status_str,
                    levels,
                    verified_at,
                });
            }
        }
    }

    Json(AllVerificationStatus {
        pending,
        running,
        passed,
        failed,
        partial,
        entities,
    })
}

/// GET /observe/design-time/stream -- SSE stream of design-time events.
///
/// Subscribes to the design-time broadcast channel and streams events
/// as they happen (spec loaded, verification started/level/done).
async fn handle_design_time_stream(
    State(state): State<ServerState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.design_time_tx.subscribe();

    let stream = BroadcastStream::new(rx).filter_map(|result| {
        match result {
            Ok(event) => {
                let data = serde_json::to_string(&event).unwrap_or_default();
                Some(Ok(Event::default().event("design_time").data(data)))
            }
            // Lagged receiver: skip missed events and continue.
            Err(_) => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------------
// Phase: Workflow view (Temporal-like)
// ---------------------------------------------------------------------------

/// A single step in an entity's verification workflow.
#[derive(Serialize)]
struct WorkflowStep {
    step: String,
    status: String,
    passed: Option<bool>,
    timestamp: Option<String>,
    summary: Option<String>,
}

/// Per-entity workflow detail.
#[derive(Serialize)]
struct EntityWorkflow {
    entity_type: String,
    steps: Vec<WorkflowStep>,
}

/// Per-app/tenant workflow summary.
#[derive(Serialize)]
struct AppWorkflow {
    tenant: String,
    status: String,
    entities: Vec<EntityWorkflow>,
    runtime_events_count: u64,
}

/// Response for GET /observe/workflows.
#[derive(Serialize)]
struct WorkflowsResponse {
    workflows: Vec<AppWorkflow>,
}

/// GET /observe/workflows -- full workflow view per app/tenant.
///
/// Builds a Temporal-like workflow timeline from the design-time event log,
/// verification statuses, and trajectory log.
async fn handle_workflows(State(state): State<ServerState>) -> Json<WorkflowsResponse> {
    let persisted_events: Option<Vec<crate::state::DesignTimeEvent>> =
        if let Some(ref store) = state.event_store {
            let rows: Result<
                Vec<(
                    String,
                    String,
                    String,
                    String,
                    Option<String>,
                    Option<bool>,
                    Option<i16>,
                    Option<i16>,
                    chrono::DateTime<chrono::Utc>,
                )>,
                sqlx::Error,
            > = sqlx::query_as(
                "SELECT kind, entity_type, tenant, summary, level, passed, step_number, \
                        total_steps, created_at \
                 FROM design_time_events \
                 ORDER BY created_at ASC, id ASC",
            )
            .fetch_all(store.pool())
            .await;
            match rows {
                Ok(rows) => Some(
                    rows.into_iter()
                        .map(
                            |(
                                kind,
                                entity_type,
                                tenant,
                                summary,
                                level,
                                passed,
                                step_number,
                                total_steps,
                                created_at,
                            )| crate::state::DesignTimeEvent {
                                kind,
                                entity_type,
                                tenant,
                                summary,
                                level,
                                passed,
                                timestamp: created_at.to_rfc3339(),
                                step_number: step_number.map(|n| n as u8),
                                total_steps: total_steps.map(|n| n as u8),
                            },
                        )
                        .collect(),
                ),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read design_time_events from postgres");
                    None
                }
            }
        } else {
            None
        };

    let runtime_counts: std::collections::BTreeMap<String, u64> =
        if let Some(ref store) = state.event_store {
            let rows: Result<Vec<(String, i64)>, sqlx::Error> = sqlx::query_as(
                "SELECT tenant, COUNT(*) AS count \
                 FROM trajectories \
                 GROUP BY tenant",
            )
            .fetch_all(store.pool())
            .await;
            match rows {
                Ok(rows) => rows
                    .into_iter()
                    .map(|(tenant, count)| (tenant, count as u64))
                    .collect(),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read trajectory counts from postgres");
                    let trajectory_log = state
                        .trajectory_log
                        .read()
                        .unwrap_or_else(|err| err.into_inner());
                    let mut counts = std::collections::BTreeMap::new();
                    for entry in trajectory_log.entries() {
                        *counts.entry(entry.tenant.clone()).or_insert(0) += 1;
                    }
                    counts
                }
            }
        } else {
            let trajectory_log = state
                .trajectory_log
                .read()
                .unwrap_or_else(|err| err.into_inner());
            let mut counts = std::collections::BTreeMap::new();
            for entry in trajectory_log.entries() {
                *counts.entry(entry.tenant.clone()).or_insert(0) += 1;
            }
            counts
        };

    let event_log: Vec<crate::state::DesignTimeEvent> = persisted_events.unwrap_or_else(|| {
        state
            .design_time_log
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    });
    let registry = state.registry.read().unwrap();

    let mut workflows = Vec::new();

    for tenant_id in registry.tenant_ids() {
        let tenant_str = tenant_id.as_str().to_string();

        // Skip system tenant in workflow view
        if tenant_str == "system" {
            continue;
        }

        let mut entity_workflows = Vec::new();
        let mut tenant_status = "completed";

        for entity_type in registry.entity_types(tenant_id) {
            // Build steps from event log
            let entity_events: Vec<_> = event_log
                .iter()
                .filter(|e| e.tenant == tenant_str && e.entity_type == entity_type)
                .collect();

            let mut steps: Vec<WorkflowStep> = Vec::new();

            // Step 1: loaded
            let loaded_event = entity_events.iter().find(|e| e.kind == "spec_loaded");
            steps.push(WorkflowStep {
                step: "loaded".to_string(),
                status: if loaded_event.is_some() {
                    "completed"
                } else {
                    "pending"
                }
                .to_string(),
                passed: None,
                timestamp: loaded_event.map(|e| e.timestamp.clone()),
                summary: loaded_event.map(|e| e.summary.clone()),
            });

            // Step 2: verify_started
            let started_event = entity_events.iter().find(|e| e.kind == "verify_started");
            steps.push(WorkflowStep {
                step: "verify_started".to_string(),
                status: if started_event.is_some() {
                    "completed"
                } else {
                    "pending"
                }
                .to_string(),
                passed: None,
                timestamp: started_event.map(|e| e.timestamp.clone()),
                summary: started_event.map(|e| e.summary.clone()),
            });

            // Steps 3-6: L0-L3 from verify_level events
            let level_events: Vec<_> = entity_events
                .iter()
                .filter(|e| e.kind == "verify_level")
                .collect();

            let level_labels = [
                "L0_symbolic",
                "L1_model_check",
                "L2_simulation",
                "L3_property_test",
            ];
            for (i, label) in level_labels.iter().enumerate() {
                let level_event = level_events.get(i);
                let status = match level_event {
                    Some(_) => "completed",
                    None => {
                        // Check if verification is still running
                        if let Some(VerificationStatus::Running) =
                            registry.get_verification_status(tenant_id, entity_type)
                        {
                            if (i == 0 && started_event.is_some() && level_events.is_empty())
                                || level_events.len() == i
                            {
                                "running"
                            } else {
                                "pending"
                            }
                        } else {
                            "pending"
                        }
                    }
                };
                steps.push(WorkflowStep {
                    step: label.to_string(),
                    status: status.to_string(),
                    passed: level_event.and_then(|e| e.passed),
                    timestamp: level_event.map(|e| e.timestamp.clone()),
                    summary: level_event.map(|e| e.summary.clone()),
                });
            }

            // Step 7: deployed
            let done_event = entity_events.iter().find(|e| e.kind == "verify_done");
            let deploy_status = match registry.get_verification_status(tenant_id, entity_type) {
                Some(VerificationStatus::Completed(result)) => {
                    if result.all_passed {
                        "completed"
                    } else {
                        "failed"
                    }
                }
                Some(VerificationStatus::Running) => {
                    tenant_status = "verifying";
                    "running"
                }
                Some(VerificationStatus::Pending) | None => {
                    if tenant_status != "verifying" {
                        tenant_status = "loading";
                    }
                    "pending"
                }
            };
            steps.push(WorkflowStep {
                step: "deployed".to_string(),
                status: deploy_status.to_string(),
                passed: done_event.and_then(|e| e.passed),
                timestamp: done_event.map(|e| e.timestamp.clone()),
                summary: done_event.map(|e| e.summary.clone()).or_else(|| {
                    if deploy_status == "completed" {
                        Some("Entity ready for runtime".to_string())
                    } else if deploy_status == "failed" {
                        Some("Verification failed".to_string())
                    } else {
                        None
                    }
                }),
            });

            // Update tenant status if any entity failed
            if deploy_status == "failed" && tenant_status != "verifying" {
                tenant_status = "failed";
            }

            entity_workflows.push(EntityWorkflow {
                entity_type: entity_type.to_string(),
                steps,
            });
        }

        // Count runtime events for this tenant
        let runtime_count = *runtime_counts.get(&tenant_str).unwrap_or(&0);

        workflows.push(AppWorkflow {
            tenant: tenant_str,
            status: tenant_status.to_string(),
            entities: entity_workflows,
            runtime_events_count: runtime_count,
        });
    }

    Json(WorkflowsResponse { workflows })
}

// ---------------------------------------------------------------------------
// Phase 6: Health & Metrics
// ---------------------------------------------------------------------------

/// GET /observe/health -- server health summary.
async fn handle_health(State(state): State<ServerState>) -> Json<serde_json::Value> {
    let now = sim_now();
    let uptime = (now - state.start_time).num_seconds().max(0) as u64;

    let specs_loaded = {
        let registry = state.registry.read().unwrap_or_else(|e| e.into_inner());
        let mut count: u64 = 0;
        for tid in registry.tenant_ids() {
            count += registry.entity_types(tid).len() as u64;
        }
        count
    };

    let active_entities = {
        let reg = state
            .actor_registry
            .read()
            .unwrap_or_else(|e| e.into_inner());
        reg.len() as u64
    };

    let transitions_total = state.metrics.transitions_total.load(Ordering::Relaxed);
    let errors_total = state.metrics.errors_total.load(Ordering::Relaxed);

    let event_store_type = if state.event_store.is_some() {
        "postgres"
    } else {
        "none"
    };

    Json(serde_json::json!({
        "status": "healthy",
        "uptime_seconds": uptime,
        "specs_loaded": specs_loaded,
        "active_entities": active_entities,
        "transitions_total": transitions_total,
        "errors_total": errors_total,
        "event_store": event_store_type,
    }))
}

/// GET /observe/metrics -- Prometheus text-format metrics.
async fn handle_metrics(
    State(state): State<ServerState>,
) -> (StatusCode, [(String, String); 1], String) {
    let mut lines = Vec::new();

    // -- temper_transitions_total --
    lines.push("# HELP temper_transitions_total Total entity state transitions.".to_string());
    lines.push("# TYPE temper_transitions_total counter".to_string());
    if let Ok(map) = state.metrics.transitions.read() {
        for (key, count) in map.iter() {
            // key format: "entity_type:action:true|false"
            let parts: Vec<&str> = key.splitn(3, ':').collect();
            if parts.len() == 3 {
                lines.push(format!(
                    "temper_transitions_total{{entity_type=\"{}\",action=\"{}\",success=\"{}\"}} {}",
                    parts[0], parts[1], parts[2], count
                ));
            }
        }
    }

    // -- temper_guard_rejections_total (subset: success=false) --
    lines.push("# HELP temper_guard_rejections_total Total failed transitions (guard not met or unknown action).".to_string());
    lines.push("# TYPE temper_guard_rejections_total counter".to_string());
    if let Ok(map) = state.metrics.transitions.read() {
        for (key, count) in map.iter() {
            if key.ends_with(":false") {
                let parts: Vec<&str> = key.splitn(3, ':').collect();
                if parts.len() == 3 {
                    lines.push(format!(
                        "temper_guard_rejections_total{{entity_type=\"{}\",action=\"{}\"}} {}",
                        parts[0], parts[1], count
                    ));
                }
            }
        }
    }

    // -- temper_active_entities --
    lines.push(
        "# HELP temper_active_entities Number of currently active entity actors.".to_string(),
    );
    lines.push("# TYPE temper_active_entities gauge".to_string());
    {
        // Count per entity_type from the entity index.
        let index = state.entity_index.read().unwrap_or_else(|e| e.into_inner());
        for (key, ids) in index.iter() {
            // key format: "tenant:entity_type"
            if let Some(entity_type) = key.split(':').nth(1) {
                lines.push(format!(
                    "temper_active_entities{{entity_type=\"{}\"}} {}",
                    entity_type,
                    ids.len()
                ));
            }
        }
    }

    lines.push(String::new()); // trailing newline
    let body = lines.join("\n");

    (
        StatusCode::OK,
        [(
            "Content-Type".to_string(),
            "text/plain; version=0.0.4; charset=utf-8".to_string(),
        )],
        body,
    )
}

// ---------------------------------------------------------------------------
// Phase 3: Trajectory tracking & failed intent capture
// ---------------------------------------------------------------------------

/// Query parameters for the trajectory aggregation endpoint.
#[derive(Deserialize)]
pub struct TrajectoryQueryParams {
    /// Filter by entity type.
    pub entity_type: Option<String>,
    /// Filter by action name.
    pub action: Option<String>,
    /// Filter by success/failure ("true" or "false").
    pub success: Option<String>,
    /// Maximum number of failed intents to return in the response (default: 50).
    pub failed_limit: Option<usize>,
}

/// GET /observe/trajectories -- aggregated trajectory stats with failed intent details.
///
/// Returns:
/// - `total`: total matching entries
/// - `success_count` / `error_count` / `success_rate`
/// - `by_action`: per-action breakdown
/// - `failed_intents`: most recent failed entries (up to `failed_limit`)
async fn handle_trajectories(
    State(state): State<ServerState>,
    Query(params): Query<TrajectoryQueryParams>,
) -> Json<serde_json::Value> {
    let failed_limit = params.failed_limit.unwrap_or(50).min(500);
    let success_filter: Option<bool> = params.success.as_deref().map(|s| s == "true");
    if let Some(ref store) = state.event_store {
        let totals: Result<(i64, i64), sqlx::Error> = sqlx::query_as(
            "SELECT COUNT(*) AS total, \
                    COALESCE(SUM(CASE WHEN success THEN 1 ELSE 0 END), 0) AS success_count \
             FROM trajectories \
             WHERE ($1::text IS NULL OR entity_type = $1) \
               AND ($2::text IS NULL OR action = $2) \
               AND ($3::bool IS NULL OR success = $3)",
        )
        .bind(params.entity_type.as_deref())
        .bind(params.action.as_deref())
        .bind(success_filter)
        .fetch_one(store.pool())
        .await;

        let by_action_rows: Result<Vec<(String, i64, i64, i64)>, sqlx::Error> = sqlx::query_as(
            "SELECT action, \
                    COUNT(*) AS total, \
                    COALESCE(SUM(CASE WHEN success THEN 1 ELSE 0 END), 0) AS success, \
                    COALESCE(SUM(CASE WHEN NOT success THEN 1 ELSE 0 END), 0) AS error \
             FROM trajectories \
             WHERE ($1::text IS NULL OR entity_type = $1) \
               AND ($2::text IS NULL OR action = $2) \
               AND ($3::bool IS NULL OR success = $3) \
             GROUP BY action \
             ORDER BY action ASC",
        )
        .bind(params.entity_type.as_deref())
        .bind(params.action.as_deref())
        .bind(success_filter)
        .fetch_all(store.pool())
        .await;

        let failed_rows: Result<
            Vec<(
                String,
                String,
                String,
                String,
                Option<String>,
                Option<String>,
                chrono::DateTime<chrono::Utc>,
            )>,
            sqlx::Error,
        > = sqlx::query_as(
            "SELECT tenant, entity_type, entity_id, action, from_status, error, created_at \
             FROM trajectories \
             WHERE success = false \
               AND ($1::text IS NULL OR entity_type = $1) \
               AND ($2::text IS NULL OR action = $2) \
               AND ($3::bool IS NULL OR success = $3) \
             ORDER BY created_at DESC \
             LIMIT $4",
        )
        .bind(params.entity_type.as_deref())
        .bind(params.action.as_deref())
        .bind(success_filter)
        .bind(failed_limit as i64)
        .fetch_all(store.pool())
        .await;

        if let (Ok((total, success_count)), Ok(by_action_rows), Ok(failed_rows)) =
            (totals, by_action_rows, failed_rows)
        {
            let total = total as u64;
            let success_count = success_count as u64;
            let error_count = total.saturating_sub(success_count);
            let success_rate = if total > 0 {
                success_count as f64 / total as f64
            } else {
                0.0
            };

            let by_action: std::collections::BTreeMap<String, serde_json::Value> = by_action_rows
                .into_iter()
                .map(|(action, total, success, error)| {
                    (
                        action,
                        serde_json::json!({
                            "total": total as u64,
                            "success": success as u64,
                            "error": error as u64,
                        }),
                    )
                })
                .collect();

            let failed_intents: Vec<serde_json::Value> = failed_rows
                .into_iter()
                .map(
                    |(tenant, entity_type, entity_id, action, from_status, error, created_at)| {
                        serde_json::json!({
                            "timestamp": created_at.to_rfc3339(),
                            "tenant": tenant,
                            "entity_type": entity_type,
                            "entity_id": entity_id,
                            "action": action,
                            "from_status": from_status,
                            "error": error,
                        })
                    },
                )
                .collect();

            return Json(serde_json::json!({
                "total": total,
                "success_count": success_count,
                "error_count": error_count,
                "success_rate": success_rate,
                "by_action": by_action,
                "failed_intents": failed_intents,
            }));
        }

        tracing::warn!("failed to query trajectories from postgres, falling back to in-memory log");
    }

    let log = state
        .trajectory_log
        .read()
        .unwrap_or_else(|e| e.into_inner());

    // Filter entries.
    let filtered: Vec<&TrajectoryEntry> = log
        .entries()
        .iter()
        .filter(|e| {
            if let Some(ref ft) = params.entity_type {
                if e.entity_type != *ft {
                    return false;
                }
            }
            if let Some(ref fa) = params.action {
                if e.action != *fa {
                    return false;
                }
            }
            if let Some(sf) = success_filter {
                if e.success != sf {
                    return false;
                }
            }
            true
        })
        .collect();

    let total = filtered.len() as u64;
    let success_count = filtered.iter().filter(|e| e.success).count() as u64;
    let error_count = total.saturating_sub(success_count);
    let success_rate = if total > 0 {
        success_count as f64 / total as f64
    } else {
        0.0
    };

    // Per-action breakdown (BTreeMap for deterministic order).
    let mut by_action: std::collections::BTreeMap<String, serde_json::Value> =
        std::collections::BTreeMap::new();
    for entry in &filtered {
        let stats = by_action
            .entry(entry.action.clone())
            .or_insert_with(|| serde_json::json!({"total": 0u64, "success": 0u64, "error": 0u64}));
        if let Some(obj) = stats.as_object_mut() {
            *obj.entry("total").or_insert(serde_json::json!(0)) =
                serde_json::json!(obj["total"].as_u64().unwrap_or(0) + 1);
            if entry.success {
                *obj.entry("success").or_insert(serde_json::json!(0)) =
                    serde_json::json!(obj["success"].as_u64().unwrap_or(0) + 1);
            } else {
                *obj.entry("error").or_insert(serde_json::json!(0)) =
                    serde_json::json!(obj["error"].as_u64().unwrap_or(0) + 1);
            }
        }
    }

    // Collect most recent failed intents.
    let failed_intents: Vec<serde_json::Value> = filtered
        .iter()
        .rev()
        .filter(|e| !e.success)
        .take(failed_limit)
        .map(|e| {
            serde_json::json!({
                "timestamp": e.timestamp,
                "tenant": e.tenant,
                "entity_type": e.entity_type,
                "entity_id": e.entity_id,
                "action": e.action,
                "from_status": e.from_status,
                "error": e.error,
            })
        })
        .collect();

    Json(serde_json::json!({
        "total": total,
        "success_count": success_count,
        "error_count": error_count,
        "success_rate": success_rate,
        "by_action": by_action,
        "failed_intents": failed_intents,
    }))
}

/// POST /observe/trajectories/unmet -- record an unmet user intent.
///
/// Called by the production chat proxy when a user asks for something
/// that doesn't map to any available action. This feeds the Evolution Engine.
async fn handle_unmet_intent(
    State(state): State<ServerState>,
    Json(body): Json<serde_json::Value>,
) -> Result<StatusCode, (StatusCode, String)> {
    use temper_runtime::scheduler::sim_now;

    let intent = body
        .get("action")
        .or_else(|| body.get("intent"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let tenant = body
        .get("tenant")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    let entity_type = body
        .get("entity_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let error_msg = body.get("error").and_then(|v| v.as_str()).unwrap_or("");

    let entry = crate::state::TrajectoryEntry {
        timestamp: sim_now().to_rfc3339(),
        tenant: tenant.to_string(),
        entity_type: entity_type.to_string(),
        entity_id: "".to_string(),
        action: intent.to_string(),
        success: false,
        from_status: None,
        to_status: None,
        error: Some(if error_msg.is_empty() {
            format!("Unmet intent: {intent}")
        } else {
            error_msg.to_string()
        }),
    };
    state
        .persist_trajectory_entry(&entry)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if let Ok(mut log) = state.trajectory_log.write() {
        log.push(entry.clone());
    }

    Ok(StatusCode::CREATED)
}

// ---------------------------------------------------------------------------
// Phase 4: Sentinel Anomaly Detection
// ---------------------------------------------------------------------------

/// POST /observe/sentinel/check -- trigger sentinel rule evaluation.
///
/// Evaluates all default sentinel rules against current server state.
/// Any triggered rules generate O-Records and store them in the RecordStore.
/// Returns a list of alerts (may be empty if all is healthy).
async fn handle_sentinel_check(
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

    Ok(Json(serde_json::json!({
        "alerts_count": alerts.len(),
        "alerts": results,
    })))
}

// ---------------------------------------------------------------------------
// Phase 5: Evolution Engine API
// ---------------------------------------------------------------------------

/// Query parameters for listing evolution records.
#[derive(Deserialize)]
pub struct EvolutionRecordParams {
    /// Filter by record type: "observation", "problem", "analysis", "decision", "insight".
    pub record_type: Option<String>,
    /// Filter by status: "open", "resolved", "superseded", "rejected".
    pub status: Option<String>,
}

/// GET /observe/evolution/records -- list all evolution records.
async fn list_evolution_records(
    State(state): State<ServerState>,
    Query(params): Query<EvolutionRecordParams>,
) -> Json<serde_json::Value> {
    if let Some(ref pg_store) = state.pg_record_store {
        let mut records: Vec<serde_json::Value> = Vec::new();
        let type_filter = params.record_type.as_deref();
        let status_filter = params.status.as_deref().and_then(parse_record_status);

        if type_filter.is_none() || type_filter == Some("observation") {
            if let Ok(observations) = pg_store.open_observations().await {
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
        }

        if type_filter.is_none() || type_filter == Some("insight") {
            if let Ok(insights) = pg_store.ranked_insights().await {
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
        }

        let total_observations = pg_store.count(RecordType::Observation).await.unwrap_or(0);
        let total_problems = pg_store.count(RecordType::Problem).await.unwrap_or(0);
        let total_analyses = pg_store.count(RecordType::Analysis).await.unwrap_or(0);
        let total_decisions = pg_store.count(RecordType::Decision).await.unwrap_or(0);
        let total_insights = pg_store.count(RecordType::Insight).await.unwrap_or(0);

        if type_filter.is_none() || type_filter == Some("problem") {
            if total_problems > 0 {
                records.push(serde_json::json!({
                    "record_type": "Problem",
                    "count": total_problems,
                    "note": "Use GET /observe/evolution/records/{id} for individual records",
                }));
            }
        }
        if type_filter.is_none() || type_filter == Some("analysis") {
            if total_analyses > 0 {
                records.push(serde_json::json!({
                    "record_type": "Analysis",
                    "count": total_analyses,
                    "note": "Use GET /observe/evolution/records/{id} for individual records",
                }));
            }
        }
        if type_filter.is_none() || type_filter == Some("decision") {
            if total_decisions > 0 {
                records.push(serde_json::json!({
                    "record_type": "Decision",
                    "count": total_decisions,
                    "note": "Use GET /observe/evolution/records/{id} for individual records",
                }));
            }
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

/// GET /observe/evolution/records/{id} -- get a single record with chain info.
async fn get_evolution_record(
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
pub struct DecideRequest {
    /// The decision: "approved", "rejected", or "deferred".
    pub decision: String,
    /// Who is making the decision (email or identifier).
    pub decided_by: String,
    /// Human rationale for the decision.
    pub rationale: String,
}

/// POST /observe/evolution/records/{id}/decide -- create a D-Record for a record.
///
/// The target record (by ID) must exist. Creates a DecisionRecord derived from it.
async fn handle_decide(
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

/// GET /observe/evolution/insights -- list ranked insights (I-Records).
async fn list_evolution_insights(State(state): State<ServerState>) -> Json<serde_json::Value> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use temper_runtime::ActorSystem;
    use temper_runtime::tenant::TenantId;
    use temper_spec::csdl::parse_csdl;
    use tower::ServiceExt;

    use crate::registry::SpecRegistry;

    const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

    fn test_state_with_registry() -> ServerState {
        let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            csdl,
            CSDL_XML.to_string(),
            &[("Order", ORDER_IOA)],
        );
        let system = ActorSystem::new("test-observe");
        ServerState::from_registry(system, registry)
    }

    fn build_test_app() -> Router {
        let state = test_state_with_registry();
        Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state)
    }

    #[tokio::test]
    async fn test_list_specs_returns_registered_entities() {
        let app = build_test_app();
        let response = app
            .oneshot(Request::get("/observe/specs").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let specs: Vec<SpecSummary> = serde_json::from_slice(&body).unwrap();
        assert!(!specs.is_empty());
        assert_eq!(specs[0].entity_type, "Order");
        assert!(!specs[0].states.is_empty());
        assert!(!specs[0].actions.is_empty());
        // New verification status fields should default to pending
        assert_eq!(specs[0].verification_status, "pending");
        assert!(specs[0].levels_passed.is_none());
        assert!(specs[0].levels_total.is_none());
    }

    #[tokio::test]
    async fn test_get_spec_detail_found() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/specs/Order")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let detail: SpecDetail = serde_json::from_slice(&body).unwrap();
        assert_eq!(detail.entity_type, "Order");
        assert!(!detail.states.is_empty());
        assert!(!detail.actions.is_empty());
    }

    #[tokio::test]
    async fn test_get_spec_detail_not_found() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/specs/NonExistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_entities_empty() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/entities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let entities: Vec<EntityInstanceSummary> = serde_json::from_slice(&body).unwrap();
        // No actors spawned yet, so should be empty
        assert!(entities.is_empty());
    }

    #[tokio::test]
    async fn test_entity_history_returns_events() {
        let state = test_state_with_registry();

        // Dispatch actions to build an event log.
        let r = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "order-hist-1",
                "AddItem",
                serde_json::json!({"ProductId": "p1"}),
            )
            .await;
        assert!(r.is_ok(), "AddItem failed: {r:?}");

        let r = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "order-hist-1",
                "SubmitOrder",
                serde_json::json!({}),
            )
            .await;
        assert!(r.is_ok(), "SubmitOrder failed: {r:?}");

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        let response = app
            .oneshot(
                Request::get("/observe/entities/Order/order-hist-1/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["entity_type"], "Order");
        assert_eq!(json["entity_id"], "order-hist-1");

        let events = json["events"].as_array().expect("events should be array");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["action"], "AddItem");
        assert_eq!(events[0]["from_state"], "Draft");
        assert_eq!(events[0]["to_state"], "Draft");
        assert_eq!(events[1]["action"], "SubmitOrder");
        assert_eq!(events[1]["to_state"], "Submitted");
    }

    #[tokio::test]
    async fn test_entity_history_empty_for_unknown() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/entities/Order/nonexistent/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["entity_type"], "Order");
        assert_eq!(json["entity_id"], "nonexistent");
        let events = json["events"].as_array().expect("events should be array");
        assert!(events.is_empty());
    }

    // -- Health endpoint tests --

    #[tokio::test]
    async fn test_health_returns_status() {
        let app = build_test_app();
        let response = app
            .oneshot(Request::get("/observe/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "healthy");
        assert!(json["specs_loaded"].as_u64().is_some());
        assert_eq!(json["event_store"], "none");
    }

    #[tokio::test]
    async fn test_health_counts_entities_and_transitions() {
        let state = test_state_with_registry();

        // Dispatch an action to create an entity and increment metrics.
        let r = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "health-test-1",
                "AddItem",
                serde_json::json!({}),
            )
            .await;
        assert!(r.is_ok());

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        let response = app
            .oneshot(Request::get("/observe/health").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["active_entities"], 1);
        assert_eq!(json["transitions_total"], 1);
        assert_eq!(json["errors_total"], 0);
    }

    // -- Metrics endpoint tests --

    #[tokio::test]
    async fn test_metrics_returns_prometheus_format() {
        let state = test_state_with_registry();

        // Dispatch a successful and a failed action to populate metrics.
        let _ = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "metrics-1",
                "AddItem",
                serde_json::json!({}),
            )
            .await;
        // SubmitOrder with 0 items should fail.
        let _ = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "metrics-2",
                "SubmitOrder",
                serde_json::json!({}),
            )
            .await;

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        let response = app
            .oneshot(
                Request::get("/observe/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.contains("text/plain"),
            "content-type should be text/plain, got: {ct}"
        );

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("temper_transitions_total"),
            "should contain transitions metric"
        );
        assert!(
            text.contains("temper_active_entities"),
            "should contain active entities metric"
        );
    }

    // -- Trajectory endpoint tests --

    #[tokio::test]
    async fn test_trajectories_records_success_and_failure() {
        let state = test_state_with_registry();

        // Successful action.
        let r = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "traj-1",
                "AddItem",
                serde_json::json!({"ProductId": "p1"}),
            )
            .await;
        assert!(r.is_ok());

        // Failed action (SubmitOrder on a brand-new entity with no items guard).
        let _ = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "traj-2",
                "SubmitOrder",
                serde_json::json!({}),
            )
            .await;

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        let response = app
            .oneshot(
                Request::get("/observe/trajectories")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json["total"].as_u64().unwrap() >= 2);
        assert!(json["success_count"].as_u64().unwrap() >= 1);
        assert!(json["error_count"].as_u64().unwrap() >= 1);
        assert!(json["success_rate"].as_f64().unwrap() > 0.0);
        assert!(json["success_rate"].as_f64().unwrap() < 1.0);

        // by_action should have keys for dispatched actions.
        let by_action = json["by_action"].as_object().unwrap();
        assert!(by_action.contains_key("AddItem"));

        // failed_intents should contain at least one entry.
        let failed = json["failed_intents"].as_array().unwrap();
        assert!(!failed.is_empty());
        assert!(failed[0]["error"].is_string());
    }

    #[tokio::test]
    async fn test_trajectories_filters_by_entity_type() {
        let state = test_state_with_registry();

        let _ = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "traj-f1",
                "AddItem",
                serde_json::json!({"ProductId": "p1"}),
            )
            .await;

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        // Filter for entity_type=Order should find our entry.
        let response = app
            .clone()
            .oneshot(
                Request::get("/observe/trajectories?entity_type=Order")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["total"].as_u64().unwrap() >= 1);

        // Filter for non-existent entity_type should return 0.
        let response = app
            .oneshot(
                Request::get("/observe/trajectories?entity_type=Nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total"], 0);
    }

    #[tokio::test]
    async fn test_trajectories_empty_when_no_actions() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::get("/observe/trajectories")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total"], 0);
        assert_eq!(json["success_count"], 0);
        assert_eq!(json["error_count"], 0);
        assert_eq!(json["success_rate"], 0.0);
        let failed = json["failed_intents"].as_array().unwrap();
        assert!(failed.is_empty());
    }

    // -- Sentinel endpoint tests --

    #[tokio::test]
    async fn test_sentinel_check_no_alerts_on_clean_state() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::post("/observe/sentinel/check")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["alerts_count"], 0);
        let alerts = json["alerts"].as_array().unwrap();
        assert!(alerts.is_empty());
    }

    #[tokio::test]
    async fn test_sentinel_check_detects_error_spike() {
        let state = test_state_with_registry();

        // Generate high error rate (>10%).
        for i in 0..8 {
            let _ = state
                .dispatch_tenant_action(
                    &TenantId::default(),
                    "Order",
                    &format!("sentinel-fail-{i}"),
                    "SubmitOrder",
                    serde_json::json!({}),
                )
                .await;
        }
        for i in 0..2 {
            let _ = state
                .dispatch_tenant_action(
                    &TenantId::default(),
                    "Order",
                    &format!("sentinel-pass-{i}"),
                    "AddItem",
                    serde_json::json!({"ProductId": "p1"}),
                )
                .await;
        }

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        let response = app
            .oneshot(
                Request::post("/observe/sentinel/check")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["alerts_count"].as_u64().unwrap() >= 1);

        let alerts = json["alerts"].as_array().unwrap();
        let error_alert = alerts.iter().find(|a| a["rule"] == "error_rate_spike");
        assert!(error_alert.is_some(), "should detect error rate spike");

        let alert = error_alert.unwrap();
        assert!(alert["record_id"].as_str().unwrap().starts_with("O-"));
    }

    // -- Evolution API endpoint tests --

    #[tokio::test]
    async fn test_evolution_records_empty() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::get("/observe/evolution/records")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total_observations"], 0);
        assert_eq!(json["total_decisions"], 0);
    }

    #[tokio::test]
    async fn test_evolution_records_after_sentinel() {
        let state = test_state_with_registry();

        // Generate errors to trigger sentinel.
        for i in 0..10 {
            let _ = state
                .dispatch_tenant_action(
                    &TenantId::default(),
                    "Order",
                    &format!("evo-fail-{i}"),
                    "SubmitOrder",
                    serde_json::json!({}),
                )
                .await;
        }

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        // Trigger sentinel first.
        let _ = app
            .clone()
            .oneshot(
                Request::post("/observe/sentinel/check")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Now check evolution records.
        let response = app
            .oneshot(
                Request::get("/observe/evolution/records")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["total_observations"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn test_evolution_get_record_not_found() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::get("/observe/evolution/records/O-2024-nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_evolution_decide_creates_d_record() {
        let state = test_state_with_registry();

        // Manually insert an O-Record.
        let obs = temper_evolution::ObservationRecord {
            header: temper_evolution::RecordHeader {
                id: "O-test-decide".to_string(),
                record_type: temper_evolution::RecordType::Observation,
                timestamp: sim_now(),
                created_by: "test".to_string(),
                derived_from: None,
                status: temper_evolution::RecordStatus::Open,
            },
            source: "test".to_string(),
            classification: temper_evolution::ObservationClass::ErrorRate,
            evidence_query: "test query".to_string(),
            threshold_field: None,
            threshold_value: None,
            observed_value: None,
            context: serde_json::json!({}),
        };
        state.record_store.insert_observation(obs);

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        // Create a D-Record decision.
        let response = app.clone()
            .oneshot(
                Request::post("/observe/evolution/records/O-test-decide/decide")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"decision":"approved","decided_by":"alice@example.com","rationale":"Looks good"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["record_id"].as_str().unwrap().starts_with("D-"));
        assert_eq!(json["decision"], "Approved");
        assert_eq!(json["derived_from"], "O-test-decide");
    }

    #[tokio::test]
    async fn test_evolution_decide_not_found() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::post("/observe/evolution/records/O-nonexistent/decide")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        r#"{"decision":"rejected","decided_by":"bob","rationale":"nope"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // -- Workflow endpoint tests --

    #[tokio::test]
    async fn test_workflows_returns_tenant_data() {
        let app = build_test_app();
        let response = app
            .oneshot(
                Request::get("/observe/workflows")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let workflows = json["workflows"].as_array().unwrap();
        // "default" tenant should appear (but "system" should be filtered out)
        assert!(
            workflows.iter().any(|w| w["tenant"] == "default"),
            "should contain 'default' tenant workflow"
        );
        // Check entity workflow structure
        let default_wf = workflows.iter().find(|w| w["tenant"] == "default").unwrap();
        let entities = default_wf["entities"].as_array().unwrap();
        assert!(!entities.is_empty());
        // Each entity should have 7 steps
        let order_wf = entities.iter().find(|e| e["entity_type"] == "Order");
        assert!(order_wf.is_some(), "should have Order entity workflow");
        let steps = order_wf.unwrap()["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 7, "should have 7 workflow steps");
        assert_eq!(steps[0]["step"], "loaded");
        assert_eq!(steps[6]["step"], "deployed");
    }

    // -- Load-dir endpoint tests --

    #[tokio::test]
    async fn test_load_dir_registers_specs() {
        let system = ActorSystem::new("test-load-dir");
        let registry = SpecRegistry::new();
        let state = ServerState::from_registry(system, registry);

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state.clone());

        // Use the test-fixtures/specs directory which has valid specs
        let specs_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-fixtures/specs");

        let body = serde_json::json!({
            "tenant": "test-tenant",
            "specs_dir": specs_dir.to_str().unwrap(),
        });

        let response = app
            .oneshot(
                Request::post("/observe/specs/load-dir")
                    .header("Content-Type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        // Response is NDJSON — parse each line
        let body = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        let lines: Vec<serde_json::Value> = body_str
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        // First line: specs_loaded
        assert_eq!(lines[0]["type"], "specs_loaded");
        assert_eq!(lines[0]["tenant"], "test-tenant");
        let entities = lines[0]["entities"].as_array().unwrap();
        assert!(
            !entities.is_empty(),
            "should have loaded at least one entity"
        );

        // Last line: summary
        let summary = lines.last().unwrap();
        assert_eq!(summary["type"], "summary");
        assert_eq!(summary["tenant"], "test-tenant");

        // Verify specs are in the registry
        let registry = state.registry.read().unwrap();
        let tenant_id: temper_runtime::tenant::TenantId = "test-tenant".into();
        let entity_types = registry.entity_types(&tenant_id);
        assert!(
            !entity_types.is_empty(),
            "registry should have entity types for test-tenant"
        );
    }

    #[tokio::test]
    async fn test_load_dir_missing_dir_returns_error() {
        let system = ActorSystem::new("test-load-dir-missing");
        let registry = SpecRegistry::new();
        let state = ServerState::from_registry(system, registry);

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state);

        let body = serde_json::json!({
            "tenant": "test-tenant",
            "specs_dir": "/nonexistent/path/to/specs",
        });

        let response = app
            .oneshot(
                Request::post("/observe/specs/load-dir")
                    .header("Content-Type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_load_dir_emits_design_time_events() {
        let system = ActorSystem::new("test-load-dir-events");
        let registry = SpecRegistry::new();
        let state = ServerState::from_registry(system, registry);

        let app = Router::new()
            .nest("/observe", build_observe_router())
            .with_state(state.clone());

        let specs_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-fixtures/specs");

        let body = serde_json::json!({
            "tenant": "event-tenant",
            "specs_dir": specs_dir.to_str().unwrap(),
        });

        let response = app
            .oneshot(
                Request::post("/observe/specs/load-dir")
                    .header("Content-Type", "application/json")
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        // Consume entire body to wait for verification to complete
        let _ = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
            .await
            .unwrap();

        // Check that design-time events were logged
        let log = state.design_time_log.read().unwrap();
        assert!(!log.is_empty(), "design-time log should have events");

        // Should have spec_loaded, verify_started, verify_level, and verify_done events
        let loaded_events: Vec<_> = log.iter().filter(|e| e.kind == "spec_loaded").collect();
        assert!(!loaded_events.is_empty(), "should have spec_loaded events");

        let started_events: Vec<_> = log.iter().filter(|e| e.kind == "verify_started").collect();
        assert!(
            !started_events.is_empty(),
            "should have verify_started events"
        );

        let done_events: Vec<_> = log.iter().filter(|e| e.kind == "verify_done").collect();
        assert!(!done_events.is_empty(), "should have verify_done events");
    }

    #[tokio::test]
    async fn test_evolution_insights_empty() {
        let app = build_test_app();

        let response = app
            .oneshot(
                Request::get("/observe/evolution/insights")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["total"], 0);
        let insights = json["insights"].as_array().unwrap();
        assert!(insights.is_empty());
    }
}
