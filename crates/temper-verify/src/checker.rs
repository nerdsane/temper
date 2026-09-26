//! Run exhaustive model checking on a `TemperModel`.
//!
//! This module wraps Stateright's BFS model checker and produces a
//! `VerificationResult` summarizing the outcome.

use std::collections::{HashSet, VecDeque};

use stateright::{Checker, Model};

use crate::model::ResolvedTransition;
use crate::model::semantics::{guard_may_hold, successors};
use crate::model::{TemperModel, TemperModelAction, TemperModelState};

/// A counterexample discovered during model checking.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Counterexample {
    /// The property name that was violated.
    pub property: String,
    /// The sequence of (state, action) pairs leading to the violation.
    pub trace: Vec<(TemperModelState, Option<TemperModelAction>)>,
}

/// The result of running exhaustive model checking.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
            let next_states = step(model, transition, &state);
            if next_states.is_empty() {
                continue;
            }
            covered[index] = true;
            for next in next_states {
                if visited_states.insert(next.clone()) {
                    queue.push_back(next);
                }
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

/// The states `transition` reaches from `state`: none when it is not
/// enabled, one per explored parameter assignment otherwise.
fn step(
    model: &TemperModel,
    transition: &ResolvedTransition,
    state: &TemperModelState,
) -> Vec<TemperModelState> {
    let status_ok = transition.from_states.is_empty()
        || transition
            .from_states
            .iter()
            .any(|from| from == &state.status);
    // Use `guard_may_hold` (not `evaluate_guard`) so cross-entity gated edges
    // are walked during reachability BFS: a cross-entity guard is a free
    // boolean, so the gated target state is genuinely reachable in the model.
    if !status_ok || !guard_may_hold(&transition.guard, &model.var_kinds, state) {
        return Vec::new();
    }
    successors(model, transition, state)
        .into_iter()
        .map(|(_, next)| next)
        .collect()
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
#[path = "checker_test.rs"]
mod tests;
