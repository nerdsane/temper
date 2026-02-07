//! Property-based testing from TLA+ specifications.
//!
//! This module generates random action sequences against a `TemperModel` built
//! from a TLA+ `StateMachine`, checking all invariants after every transition.
//! Two execution modes are provided:
//!
//! - [`run_prop_tests`]: A lightweight, manual random-testing loop.
//! - [`run_prop_tests_with_shrinking`]: Uses proptest's [`TestRunner`] so that
//!   failing inputs are automatically shrunk to a minimal counterexample.

use proptest::test_runner::{Config as ProptestConfig, TestRunner};
use stateright::Model;
use temper_spec::tlaplus::StateMachine;

use crate::model::{
    build_model, build_model_from_tla, TemperModel, TemperModelAction, TemperModelState,
};

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// Result of running property-based tests on a state machine.
#[derive(Debug, Clone)]
pub struct PropTestResult {
    /// Total number of test cases executed.
    pub total_cases: u64,
    /// Whether all test cases passed.
    pub passed: bool,
    /// Details about the first failure, if any.
    pub failure: Option<PropTestFailure>,
}

/// Details of a property-test failure.
#[derive(Debug, Clone)]
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
    use crate::model::InvariantKind;

    for inv in &model.invariants {
        match inv.kind {
            InvariantKind::StatusInSet => {
                if !model.states.contains(&state.status) {
                    return Err(inv.name.clone());
                }
            }
            InvariantKind::ItemCountPositive => {
                if inv.trigger_states.contains(&state.status) && state.item_count == 0 {
                    return Err(inv.name.clone());
                }
            }
            InvariantKind::Implication => {
                if inv.trigger_states.contains(&state.status) {
                    let valid_required: Vec<&String> = inv
                        .required_states
                        .iter()
                        .filter(|s| model.states.contains(s))
                        .collect();

                    if valid_required.is_empty() {
                        continue; // constrains non-status variables
                    }
                    if !valid_required.contains(&&state.status) {
                        return Err(inv.name.clone());
                    }
                }
            }
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

/// Run property-based tests on a TLA+ state machine.
///
/// Generates `num_cases` random action sequences of up to `max_steps` length,
/// checking all invariants after each step.  Uses a simple deterministic PRNG
/// seeded from the case index so that results are reproducible.
///
/// This variant builds the model from a `StateMachine` without TLA+ source,
/// so `CanXxx` guard predicates may not be fully resolved.  Prefer
/// [`run_prop_tests_from_tla`] when the raw TLA+ source is available.
pub fn run_prop_tests(
    sm: &StateMachine,
    num_cases: u64,
    max_steps: usize,
) -> PropTestResult {
    let model = build_model(sm);
    run_prop_tests_on_model(&model, num_cases, max_steps)
}

/// Run property-based tests from raw TLA+ source.
///
/// This builds the model via [`build_model_from_tla`] which fully resolves
/// `CanXxx` guard predicates, producing correct transition guards.
pub fn run_prop_tests_from_tla(
    tla_source: &str,
    num_cases: u64,
    max_steps: usize,
) -> PropTestResult {
    let model = build_model_from_tla(tla_source, 2);
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
        // Deterministic seed from case index.
        let mut rng_state: u64 = case_idx.wrapping_mul(6364136223846793005).wrapping_add(1);

        let mut state = init_state.clone();
        let mut action_seq: Vec<String> = Vec::new();

        // Check invariants on the initial state.
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
                break; // deadlock / terminal state
            }

            // Simple xorshift-style PRNG step.
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            let idx = (rng_state as usize) % actions.len();

            let action = actions[idx].clone();
            action_seq.push(action.name.clone());

            if let Some(next) = model.next_state(&state, action) {
                state = next;
            } else {
                break; // transition returned None -- should not happen for enabled actions
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

/// Run property-based tests using proptest's [`TestRunner`] for automatic
/// shrinking of failing inputs.
///
/// Each test case generates a `Vec<usize>` of length up to `max_steps`
/// (indices into the list of enabled actions). These indices are mapped to
/// valid actions at runtime (modulo the number of enabled actions). When a
/// failure is found, proptest shrinks the vector towards smaller values and
/// shorter prefixes, producing a minimal counterexample.
///
/// This variant builds the model from a `StateMachine` without TLA+ source.
/// Prefer [`run_prop_tests_with_shrinking_from_tla`] when the raw source is
/// available.
pub fn run_prop_tests_with_shrinking(
    sm: &StateMachine,
    num_cases: u32,
    max_steps: usize,
) -> PropTestResult {
    let model = build_model(sm);
    run_prop_tests_with_shrinking_on_model(&model, num_cases, max_steps)
}

/// Run property-based tests with shrinking, building the model from raw TLA+
/// source for full guard resolution.
pub fn run_prop_tests_with_shrinking_from_tla(
    tla_source: &str,
    num_cases: u32,
    max_steps: usize,
) -> PropTestResult {
    let model = build_model_from_tla(tla_source, 2);
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

    // Strategy: generate a Vec<usize> of length 0..=max_steps.
    let strategy = proptest::collection::vec(0..1000usize, 0..=max_steps);

    let init_states = model.init_states();
    let init_state = init_states[0].clone();

    let result = runner.run(&strategy, |action_indices| {
        let mut state = init_state.clone();

        // Check invariants on the initial state.
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
            // Extract the minimal failing input from the TestError.
            let (failure_msg, minimal_input) = match test_error {
                proptest::test_runner::TestError::Fail(reason, minimal_value) => {
                    (reason.to_string(), Some(minimal_value))
                }
                proptest::test_runner::TestError::Abort(reason) => {
                    (reason.to_string(), None)
                }
            };

            // Re-run the minimal input to extract the action sequence and
            // final state.
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

    // Shouldn't reach here during a real failure replay, but handle gracefully.
    ("unknown".to_string(), action_seq, format!("{state}"))
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{InvariantKind, ResolvedInvariant};

    const ORDER_TLA: &str = include_str!("../../../test-fixtures/specs/order.tla");

    // -- Test 1: Reference order spec passes ----------------------------------

    #[test]
    fn test_prop_tests_order_spec_passes() {
        let result = run_prop_tests_from_tla(ORDER_TLA, 200, 50);
        assert!(
            result.passed,
            "order spec should pass prop tests, but got failure: {:?}",
            result.failure,
        );
        assert_eq!(result.total_cases, 200);
        assert!(result.failure.is_none());
    }

    // -- Test 2: Very short sequences (1 step) pass ---------------------------

    #[test]
    fn test_prop_tests_single_step() {
        let result = run_prop_tests_from_tla(ORDER_TLA, 100, 1);
        assert!(
            result.passed,
            "single-step sequences should pass, but got failure: {:?}",
            result.failure,
        );
        assert_eq!(result.total_cases, 100);
    }

    // -- Test 3: Many cases (1000) pass ---------------------------------------

    #[test]
    fn test_prop_tests_many_cases() {
        let result = run_prop_tests_from_tla(ORDER_TLA, 1000, 30);
        assert!(
            result.passed,
            "1000 cases should pass, but got failure: {:?}",
            result.failure,
        );
        assert_eq!(result.total_cases, 1000);
    }

    // -- Test 4: Intentionally broken state machine is caught -----------------

    /// Construct a minimal state machine that will violate an invariant:
    /// - States: A, B
    /// - Transition: GoB (A -> B)
    /// - Invariant: when status is B, it must be A (impossible)
    ///
    /// The first step GoB transitions to B, violating the invariant.
    #[test]
    fn test_prop_tests_catches_broken_model() {
        let model = build_broken_model();
        let result = run_prop_tests_on_model(&model, 100, 10);
        assert!(
            !result.passed,
            "broken model should fail prop tests",
        );
        let failure = result.failure.as_ref().expect("should have failure details");
        assert!(
            !failure.invariant.is_empty(),
            "invariant name should be non-empty",
        );
        assert!(
            !failure.action_sequence.is_empty(),
            "action sequence should be non-empty",
        );
        assert!(
            !failure.final_state.is_empty(),
            "final state should be non-empty",
        );
    }

    // -- Test 5: Verify PropTestResult fields ---------------------------------

    #[test]
    fn test_prop_test_result_fields() {
        // Passing result from valid spec.
        let pass_result = run_prop_tests_from_tla(ORDER_TLA, 50, 20);
        assert!(pass_result.passed);
        assert_eq!(pass_result.total_cases, 50);
        assert!(pass_result.failure.is_none());

        // Failing result from broken model.
        let broken = build_broken_model();
        let fail_result = run_prop_tests_on_model(&broken, 50, 10);
        assert!(!fail_result.passed);
        // total_cases should be at most 50 (stopped at first failure).
        assert!(fail_result.total_cases <= 50);
        assert!(fail_result.total_cases >= 1);
        let f = fail_result.failure.unwrap();
        // Invariant name should be the one we defined.
        assert_eq!(f.invariant, "OnlyA");
        // The final state should mention "B" since GoB takes us there.
        assert!(
            f.final_state.contains('B'),
            "expected final state to contain 'B', got: {}",
            f.final_state,
        );
    }

    // -- Test 6: proptest shrinking mode passes on valid spec -----------------

    #[test]
    fn test_prop_tests_with_shrinking_passes() {
        let result = run_prop_tests_with_shrinking_from_tla(ORDER_TLA, 100, 30);
        assert!(
            result.passed,
            "shrinking mode should pass on valid spec, but got failure: {:?}",
            result.failure,
        );
    }

    // -- Test 7: proptest shrinking mode catches broken model -----------------

    #[test]
    fn test_prop_tests_with_shrinking_catches_broken() {
        let broken = build_broken_model();
        let result = run_prop_tests_with_shrinking_on_model(&broken, 100, 10);
        assert!(
            !result.passed,
            "shrinking mode should catch broken model",
        );
        let failure = result.failure.as_ref().expect("should have failure");
        assert_eq!(failure.invariant, "OnlyA");
    }

    // -----------------------------------------------------------------------
    // Helper: build a broken TemperModel that always violates an invariant
    // -----------------------------------------------------------------------

    /// Build a TemperModel with two states (A, B), one transition (GoB: A->B),
    /// and an invariant that says when status is B, it must be A (impossible).
    /// Transition GoB moves to B, which violates the invariant.
    fn build_broken_model() -> TemperModel {
        use temper_spec::tlaplus::Transition;

        let sm = StateMachine {
            module_name: "Broken".to_string(),
            states: vec!["A".to_string(), "B".to_string()],
            transitions: vec![Transition {
                name: "GoB".to_string(),
                from_states: vec!["A".to_string()],
                to_state: Some("B".to_string()),
                guard_expr: String::new(),
                has_parameters: false,
                effect_expr: String::new(),
            }],
            invariants: vec![],
            liveness_properties: vec![],
            constants: vec![],
            variables: vec!["status".to_string()],
        };

        // Build the base model, then inject our custom invariant.
        let mut model = crate::model::build_model(&sm);

        // Override invariants: when status is B, it must be A (impossible).
        model.invariants = vec![ResolvedInvariant {
            name: "OnlyA".to_string(),
            trigger_states: vec!["B".to_string()],
            required_states: vec!["A".to_string()],
            kind: InvariantKind::Implication,
        }];

        model
    }
}
