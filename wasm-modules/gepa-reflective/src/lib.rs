//! GEPA Reflective Dataset WASM module.
//!
//! Builds workflow-level reflective triplets from replay output so evolution can
//! learn from both failures and successful trajectories.

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "gepa-reflective: building workflow-level reflective dataset");

        let fields = ctx.entity_state.get("fields").unwrap_or(&ctx.entity_state);
        let skill_name = fields
            .get("SkillName")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let entity_type = fields
            .get("TargetEntityType")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        let replay = read_replay_result(&ctx, fields);
        let workflows = read_workflows(&replay);
        let trajectories = read_trajectories(
            ctx.trigger_params
                .get("Trajectories")
                .or_else(|| fields.get("Trajectories")),
        );

        let verification_feedback = read_string_list(
            ctx.trigger_params
                .get("VerificationErrors")
                .or_else(|| fields.get("VerificationErrors")),
        );

        let mut triplets: Vec<Value> = Vec::new();
        let mut completed_count = 0usize;
        let mut partial_count = 0usize;
        let mut failed_count = 0usize;

        for (idx, workflow) in workflows.iter().enumerate() {
            let trajectory_id = workflow
                .get("trajectory_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let outcome = workflow
                .get("outcome")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let actions_total = workflow
                .get("actions_total")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let actions_succeeded = workflow
                .get("actions_succeeded")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let final_state = workflow
                .get("final_state")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let agent_goal = workflow
                .get("agent_goal")
                .and_then(Value::as_str)
                .unwrap_or("unknown");

            let reasoning_chain = workflow
                .get("reasoning_chain")
                .and_then(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| extract_reasoning_chain(&trajectories, trajectory_id));

            let input = if reasoning_chain.is_empty() {
                format!(
                    "Trajectory '{trajectory_id}' goal='{agent_goal}' for entity '{entity_type}'."
                )
            } else {
                format!(
                    "Trajectory '{trajectory_id}' goal='{agent_goal}' for entity '{entity_type}'.\nReasoning chain:\n{reasoning_chain}"
                )
            };

            let output = build_output_summary(workflow, actions_total, actions_succeeded, final_state);
            let (feedback, preserve, score) = build_feedback_and_score(outcome, workflow, entity_type);

            match outcome {
                "completed" => completed_count += 1,
                "partial" => partial_count += 1,
                "failed" => failed_count += 1,
                _ => {}
            }

            triplets.push(json!({
                "input": input,
                "output": output,
                "feedback": feedback,
                "score": score,
                "preserve": preserve,
                "trajectory_id": trajectory_id,
                "turn_id": idx,
                "entity_type": entity_type,
                "outcome": outcome,
                "actions_total": actions_total,
                "actions_succeeded": actions_succeeded,
            }));
        }

        // Lowest scores first so failure repair context appears first in prompt.
        triplets.sort_by(|a, b| {
            let a_score = a.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            let b_score = b.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            a_score
                .partial_cmp(&b_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let patterns = extract_patterns(&workflows);
        let workflow_completion_rate = replay
            .get("workflow_completion_rate")
            .and_then(Value::as_f64)
            .unwrap_or_else(|| {
                let attempted = completed_count + partial_count + failed_count;
                if attempted == 0 {
                    0.0
                } else {
                    completed_count as f64 / attempted as f64
                }
            });

        let failure_count = partial_count + failed_count;
        let success_count = completed_count;

        let dataset = json!({
            "skill_name": skill_name,
            "entity_type": entity_type,
            "workflow_triplets": triplets,
            "triplets": triplets,
            "patterns": patterns,
            "verification_feedback": verification_feedback,
            "workflow_completion_rate": workflow_completion_rate,
            "workflow_counts": {
                "completed": completed_count,
                "partial": partial_count,
                "failed": failed_count,
            },
            "failure_count": failure_count,
            "success_count": success_count,
        });

        ctx.log(
            "info",
            &format!(
                "gepa-reflective: workflows completed={}, partial={}, failed={}",
                completed_count, partial_count, failed_count
            ),
        );

        Ok(json!({
            "DatasetJson": dataset.to_string(),
            "reflective_dataset": dataset,
        }))
    }
}

fn read_replay_result(ctx: &Context, fields: &Value) -> Value {
    let replay_json = ctx
        .trigger_params
        .get("ReplayResultJson")
        .or_else(|| fields.get("ReplayResultJson"))
        .or_else(|| ctx.trigger_params.get("replay_result"))
        .or_else(|| fields.get("replay_result"));

    let parsed = match replay_json {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| json!({})),
        Some(v) => v.clone(),
        None => json!({}),
    };

    parsed
        .get("replay_result")
        .cloned()
        .unwrap_or(parsed)
}

fn read_workflows(replay: &Value) -> Vec<Value> {
    if let Some(workflows) = replay.get("workflows").and_then(Value::as_array) {
        return workflows.clone();
    }

    // Legacy fallback: derive pseudo-workflows from flat action results.
    replay
        .get("action_results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .enumerate()
                .map(|(idx, action_result)| {
                    let action = action_result
                        .get("action")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let success = action_result
                        .get("success")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let from_state = action_result
                        .get("from_state")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let to_state = action_result
                        .get("to_state")
                        .and_then(Value::as_str)
                        .unwrap_or(from_state);
                    let error_kind = action_result
                        .get("error_kind")
                        .and_then(Value::as_str)
                        .unwrap_or("invalid_transition");
                    let error = action_result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("spec evaluation failed");

                    json!({
                        "trajectory_id": format!("legacy-action-{idx}"),
                        "agent_goal": "legacy-flat-action",
                        "outcome": if success { "completed" } else { "failed" },
                        "actions_total": 1,
                        "actions_succeeded": if success { 1 } else { 0 },
                        "final_state": if success { to_state } else { from_state },
                        "breakdown": if success {
                            Value::Null
                        } else {
                            json!({
                                "turn_index": idx,
                                "action": action,
                                "from_state": from_state,
                                "error_kind": error_kind,
                                "message": error,
                            })
                        },
                        "errors": if success {
                            Value::Array(vec![])
                        } else {
                            json!([{
                                "turn_index": idx,
                                "action": action,
                                "from_state": from_state,
                                "error_kind": error_kind,
                                "message": error,
                            }])
                        },
                        "action_sequence": [action],
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn read_trajectories(value: Option<&Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(arr)) => arr.clone(),
        Some(Value::String(s)) => {
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                match parsed {
                    Value::Array(arr) => arr,
                    Value::Object(_) => vec![parsed],
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            }
        }
        Some(Value::Object(_)) => vec![value.cloned().unwrap_or_else(|| json!({}))],
        _ => Vec::new(),
    }
}

fn extract_reasoning_chain(trajectories: &[Value], target_id: &str) -> String {
    for trajectory in trajectories {
        let metadata = trajectory.get("metadata").unwrap_or(trajectory);
        let trajectory_id = metadata
            .get("trajectory_id")
            .or_else(|| metadata.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        if trajectory_id != target_id {
            continue;
        }

        let turns = trajectory
            .get("turns")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut snippets = Vec::new();
        for (turn_idx, turn) in turns.iter().enumerate() {
            if let Some(decisions) = turn.get("decisions").and_then(Value::as_array) {
                for decision in decisions {
                    if let Some(reasoning) = decision
                        .get("reasoning")
                        .and_then(Value::as_str)
                        .or_else(|| {
                            decision
                                .get("choice")
                                .and_then(|choice| choice.get("rationale"))
                                .and_then(Value::as_str)
                        })
                        && !reasoning.trim().is_empty()
                    {
                        snippets.push(format!("turn {}: {}", turn_idx + 1, reasoning.trim()));
                    }
                }
            }

            if let Some(messages) = turn.get("messages").and_then(Value::as_array) {
                for message in messages {
                    let role = message
                        .get("role")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if role != "assistant" {
                        continue;
                    }

                    if let Some(reasoning) = message.get("reasoning").and_then(Value::as_str)
                        && !reasoning.trim().is_empty()
                    {
                        snippets.push(format!("turn {}: {}", turn_idx + 1, reasoning.trim()));
                    }

                    if let Some(text) = message
                        .get("content")
                        .and_then(|content| content.get("text"))
                        .and_then(Value::as_str)
                        && !text.trim().is_empty()
                    {
                        let trimmed = text.trim();
                        let clipped = if trimmed.len() > 320 {
                            &trimmed[..320]
                        } else {
                            trimmed
                        };
                        snippets.push(format!("turn {}: {}", turn_idx + 1, clipped));
                    }
                }
            }
        }

        return snippets.join("\n");
    }

    String::new()
}

fn build_output_summary(
    workflow: &Value,
    actions_total: u64,
    actions_succeeded: u64,
    final_state: &str,
) -> String {
    let outcome = workflow
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    let mut summary = format!(
        "Outcome={outcome}, actions_succeeded={actions_succeeded}/{actions_total}, final_state={final_state}."
    );

    if let Some(errors) = workflow.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        let first_error = &errors[0];
        let action = first_error
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let from_state = first_error
            .get("from_state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let error_kind = first_error
            .get("error_kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let message = first_error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("spec evaluation failed");
        summary.push_str(&format!(
            " First failure: action='{action}' from_state='{from_state}' error_kind='{error_kind}' message='{message}'."
        ));
    }

    summary
}

fn build_feedback_and_score(outcome: &str, workflow: &Value, entity_type: &str) -> (String, bool, f64) {
    match outcome {
        "completed" => {
            let actions_total = workflow
                .get("actions_total")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            (
                format!(
                    "PRESERVE: This workflow completed successfully ({actions_total} actions). Preserve this behavior and do not regress it."
                ),
                true,
                1.0,
            )
        }
        "partial" => {
            let suggestion = mutation_suggestion_from_breakdown(workflow, entity_type)
                .unwrap_or_else(|| {
                    "FIX: Workflow partially succeeded before failing. Add missing transitions/guards for the breakdown state-action pair while preserving successful steps."
                        .to_string()
                });
            (suggestion, false, 0.5)
        }
        _ => {
            let suggestion = mutation_suggestion_from_breakdown(workflow, entity_type)
                .unwrap_or_else(|| {
                    "FIX: Workflow failed at the beginning. Add the missing capability or valid transition for the first action."
                        .to_string()
                });
            (suggestion, false, 0.0)
        }
    }
}

fn mutation_suggestion_from_breakdown(workflow: &Value, entity_type: &str) -> Option<String> {
    let breakdown = workflow.get("breakdown")?;
    let action = breakdown
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let from_state = breakdown
        .get("from_state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let error_kind = breakdown
        .get("error_kind")
        .and_then(Value::as_str)
        .unwrap_or("invalid_transition");

    let suggestion = match error_kind {
        "unknown_action" => format!(
            "FIX: Add [[action]] section '{action}' to the {entity_type} spec with 'from' including '{from_state}' and a valid 'to' state."
        ),
        "guard_rejection" => format!(
            "FIX: Relax or correct guards for action '{action}' from state '{from_state}' so valid workflows are not blocked."
        ),
        _ => format!(
            "FIX: Update action '{action}' to allow transition from '{from_state}' (add '{from_state}' to the action's 'from' states or correct transition topology)."
        ),
    };

    Some(suggestion)
}

fn extract_patterns(workflows: &[Value]) -> Value {
    let mut failure_counts: std::collections::BTreeMap<(String, String), u64> =
        std::collections::BTreeMap::new();
    let mut missing_capabilities: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    let mut guard_friction_counts: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();
    let mut successful_patterns: Vec<Value> = Vec::new();

    for workflow in workflows {
        let outcome = workflow
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        if outcome == "completed" {
            let seq = workflow
                .get("action_sequence")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let actions: Vec<String> = seq
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if !actions.is_empty() {
                successful_patterns.push(json!({
                    "trajectory_id": workflow
                        .get("trajectory_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown"),
                    "actions": actions,
                }));
            }
        }

        if let Some(errors) = workflow.get("errors").and_then(Value::as_array) {
            for error in errors {
                let action = error
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let from_state = error
                    .get("from_state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let error_kind = error
                    .get("error_kind")
                    .and_then(Value::as_str)
                    .unwrap_or("invalid_transition");

                *failure_counts
                    .entry((action.clone(), from_state.clone()))
                    .or_insert(0) += 1;

                if error_kind == "unknown_action" {
                    missing_capabilities.insert(action.clone());
                }
                if error_kind == "guard_rejection" {
                    let key = format!("{action} from {from_state}");
                    *guard_friction_counts.entry(key).or_insert(0) += 1;
                }
            }
        }
    }

    let mut common_failure_points: Vec<Value> = failure_counts
        .into_iter()
        .map(|((action, from_state), occurrences)| {
            json!({
                "action": action,
                "from_state": from_state,
                "occurrences": occurrences,
            })
        })
        .collect();
    common_failure_points.sort_by(|a, b| {
        let oa = a.get("occurrences").and_then(Value::as_u64).unwrap_or(0);
        let ob = b.get("occurrences").and_then(Value::as_u64).unwrap_or(0);
        ob.cmp(&oa)
    });

    let guard_friction: Vec<Value> = guard_friction_counts
        .into_iter()
        .map(|(pair, occurrences)| json!({"pair": pair, "occurrences": occurrences}))
        .collect();

    json!({
        "common_failure_points": common_failure_points,
        "missing_capabilities": missing_capabilities.into_iter().collect::<Vec<_>>(),
        "guard_friction": guard_friction,
        "successful_patterns": successful_patterns,
    })
}

fn read_string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}
