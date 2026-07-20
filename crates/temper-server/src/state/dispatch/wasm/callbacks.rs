//! Spec evaluation and progress callback adapters.

use super::*;

/// Build a spec evaluator closure that uses `temper-jit` to evaluate transitions.
///
/// This bridges `temper-wasm` (no jit dep) and `temper-jit` (transition evaluation)
/// through a function pointer injected into `ProductionWasmHost`.
pub(super) fn spec_evaluator_fn() -> temper_wasm::SpecEvaluatorFn {
    use temper_jit::table::TransitionTable;
    use temper_spec::automaton::parse_automaton;

    std::sync::Arc::new(
        |ioa_source: &str, current_state: &str, action: &str, _params_json: &str| {
            let automaton = parse_automaton(ioa_source)
                .map_err(|e| format!("failed to parse IOA spec: {e}"))?;
            let table = TransitionTable::from_automaton(&automaton);

            // evaluate(current_state, item_count, action) -> Option<TransitionResult>
            match table.evaluate(current_state, 0, action) {
                Some(result) => {
                    let json = serde_json::json!({
                        "success": result.success,
                        "new_state": result.new_state,
                        "error": serde_json::Value::Null,
                    });
                    Ok(json.to_string())
                }
                None => {
                    let json = serde_json::json!({
                        "success": false,
                        "new_state": serde_json::Value::Null,
                        "error": format!("unknown action '{}' in state '{}'", action, current_state),
                    });
                    Ok(json.to_string())
                }
            }
        },
    )
}

pub(super) fn progress_emitter_fn(
    state: crate::state::ServerState,
    tenant: String,
    entity_type: String,
    entity_id: String,
    module_name: String,
) -> ProgressEmitterFn {
    std::sync::Arc::new(move |event_json: &str| {
        let parsed = serde_json::from_str::<Value>(event_json).unwrap_or_else(|_| {
            serde_json::json!({
                "kind": "integration_progress",
                "message": event_json,
            })
        });
        let kind = parsed
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("integration_progress")
            .to_string();
        let seq = state.next_entity_event_sequence(&tenant, &entity_type, &entity_id);
        let event = crate::state::AgentProgressEvent {
            tenant: tenant.clone(),
            entity_type: entity_type.clone(),
            entity_id: entity_id.clone(),
            seq,
            kind,
            agent_id: entity_id.clone(),
            tool_call_id: parsed
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            tool_name: parsed
                .get("tool_name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some(module_name.clone())),
            task_id: parsed
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            message: parsed
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string),
            timestamp: sim_now().to_rfc3339(),
            data: Some(parsed),
        };
        state.broadcast_agent_progress(event);
        Ok(())
    })
}
