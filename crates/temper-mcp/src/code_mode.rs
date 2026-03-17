//! Code Mode tools: server-independent spec verification and transition simulation.
//!
//! These tools require no running Temper server — they work entirely in-process
//! using `temper-spec` for parsing, `temper-jit` for TransitionTable construction,
//! and `temper-verify` for L1 model checking.

use serde_json::{Value, json};
use temper_jit::TransitionTable;
use temper_spec::{lint_automaton, parse_automaton};
use temper_verify::{build_model_from_ioa, check_model};

/// Parse and verify an IOA spec snippet locally (L0 + L1). No server required.
///
/// Returns structured JSON with per-level pass/fail, errors, lint findings, and
/// model-check counterexamples when invariants are violated.
pub fn verify_spec(spec: &str) -> Value {
    // --- L0: Parse ---
    let automaton = match parse_automaton(spec) {
        Ok(a) => a,
        Err(parse_err) => {
            return json!({
                "passed": false,
                "level": "L0_parse",
                "error": parse_err.to_string(),
                "lint": [],
                "model_check": null,
            });
        }
    };

    // --- L0: Lint ---
    let lint_findings = lint_automaton(&automaton);
    let lint_errors: Vec<_> = lint_findings
        .iter()
        .filter(|f| matches!(f.severity, temper_spec::LintSeverity::Error))
        .collect();

    let lint_json: Vec<Value> = lint_findings
        .iter()
        .map(|f| {
            json!({
                "code": f.code,
                "severity": format!("{:?}", f.severity),
                "message": f.message,
            })
        })
        .collect();

    if !lint_errors.is_empty() {
        return json!({
            "passed": false,
            "level": "L0_lint",
            "error": format!("{} lint error(s)", lint_errors.len()),
            "lint": lint_json,
            "model_check": null,
        });
    }

    // --- L0: TransitionTable build (catches structural issues) ---
    let _table = match TransitionTable::try_from_ioa_source(spec) {
        Ok(t) => t,
        Err(build_err) => {
            return json!({
                "passed": false,
                "level": "L0_table_build",
                "error": build_err,
                "lint": lint_json,
                "model_check": null,
            });
        }
    };

    // --- L1: Model check ---
    let model = match build_model_from_ioa(spec, 5) {
        Ok(m) => m,
        Err(model_err) => {
            return json!({
                "passed": false,
                "level": "L1_model_build",
                "error": model_err.to_string(),
                "lint": lint_json,
                "model_check": null,
            });
        }
    };

    let result = check_model(&model);

    let counterexamples: Vec<Value> = result
        .counterexamples
        .iter()
        .map(|cx| {
            json!({
                "property": cx.property,
                "trace_length": cx.trace.len(),
            })
        })
        .collect();

    let mc_json = json!({
        "states_explored": result.states_explored,
        "all_properties_hold": result.all_properties_hold,
        "is_complete": result.is_complete,
        "dead_transitions": result.dead_transitions,
        "counterexamples": counterexamples,
    });

    if result.all_properties_hold {
        json!({
            "passed": true,
            "level": "L1_model_check",
            "error": null,
            "lint": lint_json,
            "model_check": mc_json,
        })
    } else {
        json!({
            "passed": false,
            "level": "L1_model_check",
            "error": format!("{} invariant(s) violated", result.counterexamples.len()),
            "lint": lint_json,
            "model_check": mc_json,
        })
    }
}

/// Simulate a single transition locally using TransitionTable::evaluate. No server required.
///
/// Returns the resulting status, effects, and whether the transition succeeded.
/// `item_count` is an optional hint for item-count guards (defaults to 0).
pub fn simulate_transition(
    spec: &str,
    from_status: &str,
    action: &str,
    item_count: usize,
) -> Value {
    let table = match TransitionTable::try_from_ioa_source(spec) {
        Ok(t) => t,
        Err(build_err) => {
            return json!({
                "success": false,
                "error": build_err,
                "from_status": from_status,
                "action": action,
                "to_status": null,
                "effects": [],
            });
        }
    };

    // Validate the from_status exists in the spec.
    if !table.states.iter().any(|s| s == from_status) {
        return json!({
            "success": false,
            "error": format!("unknown from_status '{}'; valid states: {}", from_status, table.states.join(", ")),
            "from_status": from_status,
            "action": action,
            "to_status": null,
            "effects": [],
        });
    }

    match table.evaluate(from_status, item_count, action) {
        None => json!({
            "success": false,
            "error": format!("unknown action '{}' in spec", action),
            "from_status": from_status,
            "action": action,
            "to_status": null,
            "effects": [],
        }),
        Some(result) => {
            let effects: Vec<String> = result.effects.iter().map(|e| format!("{e:?}")).collect();
            json!({
                "success": result.success,
                // Note: TransitionResult does not carry the specific failure reason (state
                // mismatch vs. guard failure). Both produce success=false. The message is
                // intentionally generic.
                "error": if result.success { Value::Null } else {
                    json!(format!("transition '{}' not allowed from '{}'", action, from_status))
                },
                "from_status": from_status,
                "action": action,
                "to_status": result.new_state,
                "effects": effects,
            })
        }
    }
}
