//! Verification, simulation, and workflow endpoints.

use std::convert::Infallible;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::{Deserialize, Serialize};
use temper_runtime::scheduler::sim_now;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::registry::VerificationStatus;
use crate::state::ServerState;

use super::SimQueryParams;

/// POST /observe/verify/{entity} -- run verification cascade on a spec.
///
/// Runs all levels (L0 SMT, L1 Model Check, L2 DST, L3 PropTest) and returns results.
/// Emits per-level `DesignTimeEvent`s via SSE so the UI can show streaming progress.
pub(crate) async fn run_verification(
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
            .map(|l| {
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
                        && let Some(failure) = &pt.failure
                    {
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
                    if dets.is_empty() { None } else { Some(dets) }
                } else {
                    None
                };

                crate::registry::EntityLevelSummary {
                    level: l.level.to_string(),
                    passed: l.passed,
                    summary: l.summary.clone(),
                    details,
                }
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
pub(crate) async fn run_simulation(
    State(state): State<ServerState>,
    Path(entity): Path<String>,
    Query(params): Query<SimQueryParams>,
) -> Result<Json<temper_verify::SimulationResult>, StatusCode> {
    let ioa_source = {
        let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
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

// ---------------------------------------------------------------------------
// Design-time observation: verification status & SSE stream
// ---------------------------------------------------------------------------

/// Response shape for GET /observe/verification-status.
#[derive(Serialize)]
pub(super) struct AllVerificationStatus {
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
pub(crate) async fn handle_verification_status(
    State(state): State<ServerState>,
) -> Json<AllVerificationStatus> {
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock
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
pub(crate) async fn handle_design_time_stream(
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
pub(super) struct WorkflowsResponse {
    workflows: Vec<AppWorkflow>,
}

/// GET /observe/workflows -- full workflow view per app/tenant.
///
/// Builds a Temporal-like workflow timeline from the design-time event log,
/// verification statuses, and trajectory log.
pub(crate) async fn handle_workflows(State(state): State<ServerState>) -> Json<WorkflowsResponse> {
    let persisted_events: Option<Vec<crate::state::DesignTimeEvent>> = if let Some(pool) = state
        .event_store
        .as_ref()
        .and_then(|store| store.postgres_pool())
    {
        type DtEventRow = (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<bool>,
            Option<i16>,
            Option<i16>,
            chrono::DateTime<chrono::Utc>,
        );
        let rows: Result<Vec<DtEventRow>, sqlx::Error> = sqlx::query_as(
            "SELECT kind, entity_type, tenant, summary, level, passed, step_number, \
                        total_steps, created_at \
                 FROM design_time_events \
                 ORDER BY created_at ASC, id ASC",
        )
        .fetch_all(pool)
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
        if let Some(turso) = state.turso_opt() {
            match turso.load_recent_trajectories(100_000).await {
                Ok(rows) => {
                    let mut counts = std::collections::BTreeMap::new();
                    for row in &rows {
                        *counts.entry(row.tenant.clone()).or_insert(0) += 1;
                    }
                    counts
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read trajectory counts from Turso");
                    std::collections::BTreeMap::new()
                }
            }
        } else {
            std::collections::BTreeMap::new()
        };

    let event_log: Vec<crate::state::DesignTimeEvent> = if let Some(events) = persisted_events {
        events
    } else if let Some(turso) = state.turso_opt() {
        match turso.list_design_time_events(None, 10_000).await {
            Ok(rows) => rows
                .into_iter()
                .map(|r| crate::state::DesignTimeEvent {
                    kind: r.kind,
                    entity_type: r.entity_type,
                    tenant: r.tenant,
                    summary: r.summary,
                    level: r.level,
                    passed: r.passed,
                    timestamp: r.created_at,
                    step_number: r.step_number.map(|n| n as u8),
                    total_steps: r.total_steps.map(|n| n as u8),
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "failed to read design_time_events from Turso");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let registry = state.registry.read().unwrap(); // ci-ok: infallible lock

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

#[derive(Deserialize)]
pub(crate) struct PathsQueryParams {
    pub targets: Option<String>,
    pub max_paths: Option<usize>,
    pub max_length: Option<usize>,
}

pub(crate) async fn get_paths(
    State(state): State<ServerState>,
    Path(entity): Path<String>,
    Query(params): Query<PathsQueryParams>,
) -> Result<Json<temper_verify::PathExtractionResult>, StatusCode> {
    let ioa_source = {
        let registry = state.registry.read().expect("registry lock poisoned");
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
    let target_states: Vec<String> = params
        .targets
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let max_paths = params.max_paths.unwrap_or(5);
    let max_length = params.max_length.unwrap_or(20);
    // determinism-ok: spawn_blocking for CPU-intensive path extraction in HTTP handler
    let result = tokio::task::spawn_blocking(move || {
        let model = temper_verify::build_model_from_ioa(&ioa_source, 2);
        let config = temper_verify::PathExtractionConfig {
            target_states,
            max_paths_per_target: max_paths,
            max_path_length: max_length,
        };
        temper_verify::extract_paths(&model, &config)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(result))
}
