//! Property-based testing from I/O Automaton specifications.
//!
//! This module generates random action sequences against a `TemperModel` built
//! from an IOA specification, checking all invariants after every transition.
//! Two execution modes are provided:
//!
//! - [`run_prop_tests_from_ioa`]: A lightweight, manual random-testing loop.
//! - [`run_prop_tests_with_shrinking_from_ioa`]: Uses proptest's [`TestRunner`] so that
//!   failing inputs are automatically shrunk to a minimal counterexample.

use proptest::test_runner::{Config as ProptestConfig, TestRunner};
use stateright::Model;

use temper_spec::automaton::AssertCompareOp;

use crate::model::{
    InvariantKind, TemperModel, TemperModelAction, TemperModelState, build_model_from_ioa,
};

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// Result of running property-based tests on a state machine.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PropTestResult {
    /// Total number of test cases executed.
    pub total_cases: u64,
    /// Whether all test cases passed.
    pub passed: bool,
    /// Details about the first failure, if any.
    pub failure: Option<PropTestFailure>,
}

/// Details of a property-test failure.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PropTestFailure {
    /// Name of the invariant that was violated.
    pub invariant: String,
    /// The sequence of action names that led to the violation.
    pub action_sequence: Vec<String>,
    /// String representation of the state in which the violation was found.
    pub final_state: String,
}

// ---------------------------------------------------------------------------
// Invariant checking (shared by both modes)
// ---------------------------------------------------------------------------

/// Check all resolved invariants against a given model and state.
///
/// Returns `Ok(())` when every invariant holds, or `Err(invariant_name)` for
/// the first invariant that is violated.
fn check_invariants(model: &TemperModel, state: &TemperModelState) -> Result<(), String> {
    for inv in &model.invariants {
        let triggered = inv.trigger_states.is_empty() || inv.trigger_states.contains(&state.status);
        if !triggered {
            continue;
        }

        let violated = match &inv.kind {
            InvariantKind::StatusInSet => !model.states.contains(&state.status),
            InvariantKind::CounterPositive { var } => {
                state.counters.get(var).copied().unwrap_or(0) == 0
            }
            InvariantKind::BoolRequired { var } => {
                !state.booleans.get(var).copied().unwrap_or(false)
            }
            InvariantKind::NoFurtherTransitions => {
                let mut actions = Vec::new();
                model.actions(state, &mut actions);
                !actions.is_empty()
            }
            InvariantKind::Implication => {
                let valid_required: Vec<&String> = inv
                    .required_states
                    .iter()
                    .filter(|s| model.states.contains(s))
                    .collect();
                if valid_required.is_empty() {
                    false // constrains non-status variables
                } else {
                    !valid_required.contains(&&state.status)
                }
            }
            InvariantKind::CounterCompare { var, op, value } => {
                let val = state.counters.get(var).copied().unwrap_or(0);
                let holds = match op {
                    AssertCompareOp::Gt => val > *value,
                    AssertCompareOp::Gte => val >= *value,
                    AssertCompareOp::Lt => val < *value,
                    AssertCompareOp::Lte => val <= *value,
                    AssertCompareOp::Eq => val == *value,
                };
                !holds
            }
            InvariantKind::NeverState { state: forbidden } => {
                state.status == *forbidden
            }
            InvariantKind::Unverifiable { .. } => false, // not checkable, never violated
        };

        if violated {
            return Err(inv.name.clone());
        }
    }
    Ok(())
}

/// Collect enabled actions for a state.
fn enabled_actions(model: &TemperModel, state: &TemperModelState) -> Vec<TemperModelAction> {
    let mut actions = Vec::new();
    model.actions(state, &mut actions);
    actions
}

// ---------------------------------------------------------------------------
// Mode 1 -- lightweight manual loop
// ---------------------------------------------------------------------------

/// Run property-based tests from I/O Automaton TOML source.
pub fn run_prop_tests_from_ioa(ioa_toml: &str, num_cases: u64, max_steps: usize) -> PropTestResult {
    let model = build_model_from_ioa(ioa_toml, 2);
    run_prop_tests_on_model(&model, num_cases, max_steps)
}

/// Run property-based tests on a pre-built `TemperModel`.
pub fn run_prop_tests_on_model(
    model: &TemperModel,
    num_cases: u64,
    max_steps: usize,
) -> PropTestResult {
    let init_states = model.init_states();
    let init_state = &init_states[0];

    for case_idx in 0..num_cases {
        let mut rng_state: u64 = case_idx.wrapping_mul(6364136223846793005).wrapping_add(1);

        let mut state = init_state.clone();
        let mut action_seq: Vec<String> = Vec::new();

        if let Err(inv) = check_invariants(model, &state) {
            return PropTestResult {
                total_cases: case_idx + 1,
                passed: false,
                failure: Some(PropTestFailure {
                    invariant: inv,
                    action_sequence: action_seq,
                    final_state: format!("{state}"),
                }),
            };
        }

        for _step in 0..max_steps {
            let actions = enabled_actions(model, &state);
            if actions.is_empty() {
                break;
            }

            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            let idx = (rng_state as usize) % actions.len();

            let action = actions[idx].clone();
            action_seq.push(action.name.clone());

            if let Some(next) = model.next_state(&state, action) {
                state = next;
            } else {
                break;
            }

            if let Err(inv) = check_invariants(model, &state) {
                return PropTestResult {
                    total_cases: case_idx + 1,
                    passed: false,
                    failure: Some(PropTestFailure {
                        invariant: inv,
                        action_sequence: action_seq,
                        final_state: format!("{state}"),
                    }),
                };
            }
        }
    }

    PropTestResult {
        total_cases: num_cases,
        passed: true,
        failure: None,
    }
}

// ---------------------------------------------------------------------------
// Mode 2 -- proptest TestRunner with shrinking
// ---------------------------------------------------------------------------

/// Run property-based tests with shrinking from I/O Automaton TOML source.
pub fn run_prop_tests_with_shrinking_from_ioa(
    ioa_toml: &str,
    num_cases: u32,
    max_steps: usize,
) -> PropTestResult {
    let model = build_model_from_ioa(ioa_toml, 2);
    run_prop_tests_with_shrinking_on_model(&model, num_cases, max_steps)
}

/// Run property-based tests with shrinking on a pre-built `TemperModel`.
pub fn run_prop_tests_with_shrinking_on_model(
    model: &TemperModel,
    num_cases: u32,
    max_steps: usize,
) -> PropTestResult {
    let config = ProptestConfig {
        cases: num_cases,
        ..ProptestConfig::default()
    };

    let mut runner = TestRunner::new(config);
    let strategy = proptest::collection::vec(0..1000usize, 0..=max_steps);

    let init_states = model.init_states();
    let init_state = init_states[0].clone();

    let result = runner.run(&strategy, |action_indices| {
        let mut state = init_state.clone();

        check_invariants(model, &state).map_err(|inv| {
            proptest::test_runner::TestCaseError::Fail(
                format!("invariant {inv} violated at initial state {state}").into(),
            )
        })?;

        for &idx in &action_indices {
            let actions = enabled_actions(model, &state);
            if actions.is_empty() {
                break;
            }
            let action = actions[idx % actions.len()].clone();

            if let Some(next) = model.next_state(&state, action) {
                state = next;
            } else {
                break;
            }

            check_invariants(model, &state).map_err(|inv| {
                proptest::test_runner::TestCaseError::Fail(
                    format!("invariant {inv} violated at state {state}").into(),
                )
            })?;
        }

        Ok(())
    });

    match result {
        Ok(()) => PropTestResult {
            total_cases: num_cases as u64,
            passed: true,
            failure: None,
        },
        Err(test_error) => {
            let (failure_msg, minimal_input) = match test_error {
                proptest::test_runner::TestError::Fail(reason, minimal_value) => {
                    (reason.to_string(), Some(minimal_value))
                }
                proptest::test_runner::TestError::Abort(reason) => (reason.to_string(), None),
            };

            let (invariant, action_sequence, final_state) =
                if let Some(action_indices) = minimal_input {
                    replay_failure(model, &init_state, &action_indices)
                } else {
                    (failure_msg.clone(), vec![], String::new())
                };

            PropTestResult {
                total_cases: num_cases as u64,
                passed: false,
                failure: Some(PropTestFailure {
                    invariant,
                    action_sequence,
                    final_state,
                }),
            }
        }
    }
}

/// Replay a failing action-index sequence to recover the action names and
/// final state.
fn replay_failure(
    model: &TemperModel,
    init_state: &TemperModelState,
    action_indices: &[usize],
) -> (String, Vec<String>, String) {
    let mut state = init_state.clone();
    let mut action_seq = Vec::new();

    if let Err(inv) = check_invariants(model, &state) {
        return (inv, action_seq, format!("{state}"));
    }

    for &idx in action_indices {
        let actions = enabled_actions(model, &state);
        if actions.is_empty() {
            break;
        }
        let action = actions[idx % actions.len()].clone();
        action_seq.push(action.name.clone());

        if let Some(next) = model.next_state(&state, action) {
            state = next;
        } else {
            break;
        }

        if let Err(inv) = check_invariants(model, &state) {
            return (inv, action_seq, format!("{state}"));
        }
    }

    ("unknown".to_string(), action_seq, format!("{state}"))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InvariantKind, ModelGuard, ResolvedInvariant, ResolvedTransition};
    use std::collections::BTreeMap;

    const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

    #[test]
    fn test_prop_tests_order_spec_passes() {
        let result = run_prop_tests_from_ioa(ORDER_IOA, 200, 50);
        assert!(
            result.passed,
            "order spec should pass prop tests, but got failure: {:?}",
            result.failure,
        );
        assert_eq!(result.total_cases, 200);
        assert!(result.failure.is_none());
    }

    #[test]
    fn test_prop_tests_single_step() {
        let result = run_prop_tests_from_ioa(ORDER_IOA, 100, 1);
        assert!(
            result.passed,
            "single-step sequences should pass, but got failure: {:?}",
            result.failure,
        );
        assert_eq!(result.total_cases, 100);
    }

    #[test]
    fn test_prop_tests_many_cases() {
        let result = run_prop_tests_from_ioa(ORDER_IOA, 1000, 30);
        assert!(
            result.passed,
            "1000 cases should pass, but got failure: {:?}",
            result.failure,
        );
        assert_eq!(result.total_cases, 1000);
    }

    #[test]
    fn test_prop_tests_catches_broken_model() {
        let model = build_broken_model();
        let result = run_prop_tests_on_model(&model, 100, 10);
        assert!(!result.passed, "broken model should fail prop tests");
        let failure = result
            .failure
            .as_ref()
            .expect("should have failure details");
        assert!(!failure.invariant.is_empty());
        assert!(!failure.action_sequence.is_empty());
        assert!(!failure.final_state.is_empty());
    }

    #[test]
    fn test_prop_test_result_fields() {
        let pass_result = run_prop_tests_from_ioa(ORDER_IOA, 50, 20);
        assert!(pass_result.passed);
        assert_eq!(pass_result.total_cases, 50);
        assert!(pass_result.failure.is_none());

        let broken = build_broken_model();
        let fail_result = run_prop_tests_on_model(&broken, 50, 10);
        assert!(!fail_result.passed);
        assert!(fail_result.total_cases <= 50);
        assert!(fail_result.total_cases >= 1);
        let f = fail_result.failure.unwrap();
        assert_eq!(f.invariant, "OnlyA");
        assert!(
            f.final_state.contains('B'),
            "expected final state to contain 'B', got: {}",
            f.final_state,
        );
    }

    #[test]
    fn test_prop_tests_with_shrinking_passes() {
        let result = run_prop_tests_with_shrinking_from_ioa(ORDER_IOA, 100, 30);
        assert!(
            result.passed,
            "shrinking mode should pass on valid spec, but got failure: {:?}",
            result.failure,
        );
    }

    #[test]
    fn test_prop_tests_with_shrinking_catches_broken() {
        let broken = build_broken_model();
        let result = run_prop_tests_with_shrinking_on_model(&broken, 100, 10);
        assert!(!result.passed, "shrinking mode should catch broken model");
        let failure = result.failure.as_ref().expect("should have failure");
        assert_eq!(failure.invariant, "OnlyA");
    }

    // -----------------------------------------------------------------------
    // Helper: build a broken TemperModel that always violates an invariant
    // -----------------------------------------------------------------------

    fn build_broken_model() -> TemperModel {
        TemperModel {
            states: vec!["A".to_string(), "B".to_string()],
            transitions: vec![ResolvedTransition {
                name: "GoB".to_string(),
                from_states: vec!["A".to_string()],
                to_state: Some("B".to_string()),
                guard: ModelGuard::Always,
                effects: vec![],
            }],
            invariants: vec![
                ResolvedInvariant {
                    name: "TypeInvariant".to_string(),
                    trigger_states: vec![],
                    required_states: vec![],
                    kind: InvariantKind::StatusInSet,
                },
                ResolvedInvariant {
                    name: "OnlyA".to_string(),
                    trigger_states: vec!["B".to_string()],
                    required_states: vec!["A".to_string()],
                    kind: InvariantKind::Implication,
                },
            ],
            liveness: vec![],
            initial_status: "A".to_string(),
            initial_counters: BTreeMap::new(),
            initial_booleans: BTreeMap::new(),
            initial_lists: BTreeMap::new(),
            counter_bounds: BTreeMap::new(),
            default_max_counter: 2,
        }
    }
}
