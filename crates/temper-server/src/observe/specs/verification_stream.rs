use std::collections::BTreeMap;
use std::convert::Infallible;

use tokio_stream::wrappers::ReceiverStream;

use temper_runtime::scheduler::sim_now;

use crate::registry::VerificationStatus;
use crate::state::ServerState;

pub(super) fn build_verification_stream_response(
    state: ServerState,
    tenant: String,
    entity_names: Vec<String>,
    ioa_sources: BTreeMap<String, String>,
    lint_warning_lines: Vec<serde_json::Value>,
    cross_lint_warning_lines: Vec<serde_json::Value>,
) -> axum::response::Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, Infallible>>(100);
    let state_for_task = state.clone();

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

        for finding in &lint_warning_lines {
            let _ = tx
                .send(Ok(serde_json::to_string(finding)
                    .unwrap() // ci-ok: infallible serialization
                    + "\n"))
                .await;
        }
        for finding in &cross_lint_warning_lines {
            let _ = tx
                .send(Ok(serde_json::to_string(finding)
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
                        levels: cascade_result
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
                                        for transition in &mc.dead_transitions {
                                            dets.push(crate::registry::VerificationDetail {
                                                kind: "dead_transition".into(),
                                                property: transition.clone(),
                                                description:
                                                    "Transition is unreachable in model check"
                                                        .into(),
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
                            })
                            .collect(),
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
    axum::response::Response::builder()
        .header("content-type", "application/x-ndjson")
        .body(body)
        .unwrap() // ci-ok: Response::builder with valid headers is infallible
}
