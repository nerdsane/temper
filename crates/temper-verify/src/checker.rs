//! Run exhaustive model checking on a `TemperModel`.
//!
//! This module wraps Stateright's BFS model checker and produces a
//! `VerificationResult` summarizing the outcome.

use std::collections::{HashSet, VecDeque};

use stateright::{Checker, Model};

use crate::model::{ResolvedTransition, TemperModel, TemperModelAction, TemperModelState};

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
    check_model_with_state_budget(model, usize::MAX)
}

/// Run BFS model checking with an explicit unique-state budget.
pub fn check_model_with_state_budget(
    model: &TemperModel,
    state_budget: usize,
) -> VerificationResult {
    assert!(
        state_budget > 0,
        "model-check state budget must be positive"
    );
    let checker_result = model
        .clone()
        .checker()
        .target_state_count(state_budget)
        .spawn_bfs()
        .join();

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

    let dead_transitions = if is_complete {
        find_dead_transitions(model)
    } else {
        vec!["model-check state budget exhausted".to_string()]
    };
    let all_properties_hold =
        is_complete && counterexamples.is_empty() && dead_transitions.is_empty();

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
        let mut actions = Vec::new();
        model.actions(&state, &mut actions);
        for action in actions {
            let Some(index) = model
                .transitions
                .iter()
                .position(|transition| transition.name == action.name)
            else {
                continue;
            };
            let Some(next) = model.next_state(&state, action) else {
                continue;
            };
            covered[index] = true;
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
        let model = build_model_from_ioa(ORDER_IOA, 2).unwrap();
        let result = check_model(&model);
        assert!(result.is_complete, "checker should complete");
        assert!(
            result.states_explored > 0,
            "should explore at least one state"
        );
    }

    #[test]
    fn test_check_model_all_properties_hold() {
        let model = build_model_from_ioa(ORDER_IOA, 2).unwrap();
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
        let model = build_model_from_ioa(src, 2).unwrap();
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

    #[test]
    fn test_cross_entity_guard_does_not_break_local_terminal_proof() {
        let src = r#"
[automaton]
name = "Parent"
states = ["Waiting", "Ready"]
initial = "Waiting"

[[action]]
name = "ProceedWhenChildDone"
from = ["Waiting"]
to = "Ready"
guard = [{ type = "cross_entity_state", entity_type = "Child", entity_id_source = "child_id", required_status = ["Done"] }]

[[invariant]]
name = "WaitingLocallyTerminal"
when = ["Waiting"]
assert = "no_further_transitions"
"#;
        let model = build_model_from_ioa(src, 2).unwrap();
        let result = check_model(&model);
        assert!(
            result.all_properties_hold,
            "abstract cross-entity guard must not be treated as a locally enabled transition: {result:?}"
        );
        assert!(
            result.dead_transitions.is_empty(),
            "abstract cross-entity transitions should not be reported as dead: {:?}",
            result.dead_transitions
        );
        // The local-terminal proof still holds (no_further_transitions uses
        // local enablement, where the gate is false), AND the gated edge is now
        // genuinely explored: Ready is reachable, so the model is not silently
        // pruning the state behind the gate.
        assert!(
            states_contains_status(&model, "Ready"),
            "cross-entity gated target state must be reachable in the explored model"
        );
    }

    /// Walk the model's reachable states (mirroring the checker BFS) and report
    /// whether `status` is among them.
    fn states_contains_status(model: &TemperModel, status: &str) -> bool {
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        for init in model.init_states() {
            if visited.insert(init.clone()) {
                queue.push_back(init);
            }
        }
        while let Some(state) = queue.pop_front() {
            if state.status == status {
                return true;
            }
            let mut actions = Vec::new();
            model.actions(&state, &mut actions);
            for action in actions {
                if let Some(next) = model.next_state(&state, action)
                    && visited.insert(next.clone())
                {
                    queue.push_back(next);
                }
            }
        }
        false
    }

    #[test]
    fn test_cross_entity_gated_only_target_is_reachable_not_dead() {
        // Published is reachable ONLY through a cross-entity file-ready gate
        // (mirrors a publish transition gated on a related file entity). Before
        // the free-boolean fix the gate lowered to constant-false, so this edge
        // was vacuously dead and Published was never reached. It must now be
        // both reachable and not reported dead.
        let src = r#"
[automaton]
name = "DesignLanguage"
states = ["Draft", "Published"]
initial = "Draft"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
guard = [{ type = "cross_entity_state", entity_type = "File", entity_id_source = "file_id", required_status = ["Ready"] }]
"#;
        let model = build_model_from_ioa(src, 2).unwrap();
        let result = check_model(&model);

        assert!(
            result.dead_transitions.is_empty(),
            "Publish gated by a cross-entity guard must not be dead: {:?}",
            result.dead_transitions
        );
        assert!(
            result.all_properties_hold,
            "free-boolean cross-entity guard should keep L1 green: {result:?}"
        );
        assert!(
            states_contains_status(&model, "Published"),
            "Published must be reachable through the free-boolean cross-entity edge"
        );
    }

    #[test]
    fn test_liveness_reaches_state_through_cross_entity_gate() {
        // A liveness "eventually reaches Published" property can only be proven
        // if the gated edge is explored. With the free-boolean treatment the
        // ReachesTerminal property holds.
        let src = r#"
[automaton]
name = "DesignLanguage"
states = ["Draft", "Published"]
initial = "Draft"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
guard = [{ type = "cross_entity_state", entity_type = "File", entity_id_source = "file_id", required_status = ["Ready"] }]

[[liveness]]
name = "EventuallyPublished"
from = ["Draft"]
reaches = ["Published"]
"#;
        let model = build_model_from_ioa(src, 2).unwrap();
        let result = check_model(&model);
        assert!(
            result.all_properties_hold,
            "liveness toward a cross-entity gated state must be provable: {result:?}"
        );
    }

    #[test]
    fn test_cross_entity_transition_dead_when_status_precondition_unreachable() {
        // The free boolean only relaxes the cross-entity conjunct; a transition
        // whose from-state is genuinely never reached is still (correctly) dead.
        let src = r#"
[automaton]
name = "Orphan"
states = ["Start", "Stranded", "End"]
initial = "Start"

[[action]]
name = "Finish"
from = ["Start"]
to = "End"

[[action]]
name = "GatedFromStranded"
from = ["Stranded"]
to = "End"
guard = [{ type = "cross_entity_state", entity_type = "Other", entity_id_source = "other_id", required_status = ["Ok"] }]
"#;
        let model = build_model_from_ioa(src, 2).unwrap();
        let result = check_model(&model);
        assert!(
            result
                .dead_transitions
                .iter()
                .any(|t| t.contains("GatedFromStranded")),
            "a cross-entity transition out of an unreachable state must still be reported dead: {:?}",
            result.dead_transitions
        );
    }
}
