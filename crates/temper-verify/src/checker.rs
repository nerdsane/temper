//! Run exhaustive model checking on a `TemperModel`.
//!
//! This module wraps Stateright's BFS model checker and produces a
//! `VerificationResult` summarizing the outcome.

use std::collections::{HashSet, VecDeque};

use stateright::{Checker, Model};

use crate::model::semantics::{apply_effects, evaluate_guard};
use crate::model::{ModelEffect, ResolvedTransition};
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
    /// Transitions declared in the model that were never enabled on any reachable state.
    pub dead_transitions: Vec<String>,
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

    let dead_transitions = find_dead_transitions(model);
    let all_properties_hold = counterexamples.is_empty() && dead_transitions.is_empty();

    VerificationResult {
        states_explored,
        all_properties_hold,
        counterexamples,
        dead_transitions,
        is_complete,
    }
}

fn find_dead_transitions(model: &TemperModel) -> Vec<String> {
    let mut visited_states = HashSet::new();
    let mut queue = VecDeque::new();
    for init in model.init_states() {
        if visited_states.insert(init.clone()) {
            queue.push_back(init);
        }
    }

    let mut covered = vec![false; model.transitions.len()];

    while let Some(state) = queue.pop_front() {
        for (index, transition) in model.transitions.iter().enumerate() {
            if !is_transition_enabled(model, transition, &state) {
                continue;
            }
            covered[index] = true;

            let next = apply_transition(&state, transition);
            if visited_states.insert(next.clone()) {
                queue.push_back(next);
            }
        }
    }

    model
        .transitions
        .iter()
        .enumerate()
        .filter_map(|(index, transition)| {
            if covered[index] {
                None
            } else {
                Some(render_transition_label(transition))
            }
        })
        .collect()
}

fn is_transition_enabled(
    model: &TemperModel,
    transition: &ResolvedTransition,
    state: &TemperModelState,
) -> bool {
    let status_ok = transition.from_states.is_empty()
        || transition
            .from_states
            .iter()
            .any(|from| from == &state.status);
    if !status_ok || !evaluate_guard(&transition.guard, state) {
        return false;
    }

    for effect in &transition.effects {
        match effect {
            ModelEffect::IncrementCounter(var) => {
                let current = state.counters.get(var).copied().unwrap_or(0);
                let bound = model
                    .counter_bounds
                    .get(var)
                    .copied()
                    .unwrap_or(model.default_max_counter);
                if current >= bound {
                    return false;
                }
            }
            ModelEffect::ListAppend(var) => {
                let current_len = state.lists.get(var).map_or(0, Vec::len);
                if current_len >= model.default_max_counter {
                    return false;
                }
            }
            _ => {}
        }
    }

    true
}

fn apply_transition(state: &TemperModelState, transition: &ResolvedTransition) -> TemperModelState {
    let mut next = state.clone();
    if let Some(to_state) = &transition.to_state {
        next.status = to_state.clone();
    }
    apply_effects(&transition.effects, &mut next, &transition.name);
    next
}

fn render_transition_label(transition: &ResolvedTransition) -> String {
    let from = if transition.from_states.is_empty() {
        "*".to_string()
    } else {
        transition.from_states.join("|")
    };
    let to = transition
        .to_state
        .clone()
        .unwrap_or_else(|| "<same>".to_string());
    format!("{} [{} -> {}]", transition.name, from, to)
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

    #[test]
    fn test_check_model_finds_dead_transitions() {
        let src = r#"
[automaton]
name = "Plan"
states = ["Draft", "Active", "Completed"]
initial = "Draft"

[[state]]
name = "task_count"
type = "counter"
initial = "0"

[[action]]
name = "Activate"
from = ["Draft"]
to = "Active"

[[action]]
name = "Complete"
from = ["Active"]
to = "Completed"
guard = "task_count > 0"
"#;
        let model = build_model_from_ioa(src, 2);
        let result = check_model(&model);
        assert!(!result.all_properties_hold);
        assert!(
            result
                .dead_transitions
                .iter()
                .any(|transition| transition.contains("Complete")),
            "expected dead transition for Complete, got {:?}",
            result.dead_transitions
        );
    }
}
