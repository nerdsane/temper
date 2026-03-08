//! POST /observe/verify/{entity} -- run verification cascade on a spec.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use temper_runtime::scheduler::sim_now;
use tracing::instrument;

use crate::authz::require_observe_auth;
use crate::registry::VerificationStatus;
use crate::state::ServerState;

/// POST /observe/verify/{entity} -- run verification cascade on a spec.
///
/// Runs all levels (L0 SMT, L1 Model Check, L2 DST, L3 PropTest) and returns results.
/// Emits per-level `DesignTimeEvent`s via SSE so the UI can show streaming progress.
#[instrument(skip_all, fields(entity, otel.name = "POST /observe/verify/{entity}"))]
pub(crate) async fn handle_run_verification(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(entity): Path<String>,
) -> Result<Json<temper_verify::CascadeResult>, StatusCode> {
    require_observe_auth(&state, &headers, "run_verification", "Verification")?;

    let Some((tenant_id, ioa_source)) = state.find_entity_ioa_source(&entity) else {
        tracing::warn!("entity spec not found for verification");
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
