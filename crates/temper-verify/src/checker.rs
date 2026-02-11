//! Run exhaustive model checking on a `TemperModel`.
//!
//! This module wraps Stateright's BFS model checker and produces a
//! `VerificationResult` summarizing the outcome.

use stateright::{Checker, Model};

use crate::model::{TemperModel, TemperModelAction, TemperModelState};

/// A counterexample discovered during model checking.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Counterexample {
    /// The property name that was violated.
    pub property: String,
    /// The sequence of (state, action) pairs leading to the violation.
    pub trace: Vec<(TemperModelState, Option<TemperModelAction>)>,
}

/// The result of running exhaustive model checking.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VerificationResult {
    /// Total number of unique states explored.
    pub states_explored: usize,
    /// Whether all declared properties hold across all reachable states.
    pub all_properties_hold: bool,
    /// Counterexamples found (one per violated property).
    pub counterexamples: Vec<Counterexample>,
    /// Whether the checker completed its exploration (vs. hitting a limit).
    pub is_complete: bool,
}

/// Run exhaustive BFS model checking on the given `TemperModel`.
///
/// This spawns Stateright's BFS checker, joins it, and then inspects the
/// discoveries to build a `VerificationResult`.
pub fn check_model(model: &TemperModel) -> VerificationResult {
    let checker_result = model.clone().checker().spawn_bfs().join();

    let states_explored = checker_result.unique_state_count();
    let is_complete = checker_result.is_done();

    let discoveries = checker_result.discoveries();
    let mut counterexamples = Vec::new();

    for (property_name, path) in discoveries {
        let mut trace = Vec::new();
        let steps: Vec<_> = path.into_vec();
        for (state, action) in steps {
            trace.push((state, action));
        }
        counterexamples.push(Counterexample {
            property: property_name.to_string(),
            trace,
        });
    }

    let all_properties_hold = counterexamples.is_empty();

    VerificationResult {
        states_explored,
        all_properties_hold,
        counterexamples,
        is_complete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::build_model_from_ioa;

    const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

    #[test]
    fn test_check_model_completes() {
        let model = build_model_from_ioa(ORDER_IOA, 2);
        let result = check_model(&model);
        assert!(result.is_complete, "checker should complete");
        assert!(
            result.states_explored > 0,
            "should explore at least one state"
        );
    }

    #[test]
    fn test_check_model_all_properties_hold() {
        let model = build_model_from_ioa(ORDER_IOA, 2);
        let result = check_model(&model);
        assert!(
            result.all_properties_hold,
            "all properties should hold, but got counterexamples: {:?}",
            result.counterexamples,
        );
    }
}
