//! Local-invariant evaluator for the composite verifier.
//!
//! Projects a joint state down to a single entity's slice and evaluates
//! that entity's `ResolvedInvariant`s. Called from
//! [`super::model::CompositeTemperModel::properties`] for each entity in
//! the composition on every BFS-visited state.
//!
//! This mirrors the single-entity evaluator in
//! [`crate::model::stateright_impl`] for every invariant represented by
//! [`InvariantKind`]. Expressions classified as `Unverifiable` remain an
//! explicit verification warning because `TemperModelState` does not carry the
//! history or arbitrary data fields needed to prove them.

use crate::model::{InvariantKind, TemperModel, TemperModelState};

/// Evaluate every invariant on `model` against `state` (a single
/// entity's slice of the joint state). Returns `true` iff all pass.
pub(super) fn all_local_invariants_hold(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if !triggers_on(&inv.trigger_states, &state.status) {
            continue;
        }
        if !evaluate_one(model, &inv.kind, &inv.required_states, state) {
            return false;
        }
    }
    true
}

fn triggers_on(trigger_states: &[String], current: &str) -> bool {
    trigger_states.is_empty() || trigger_states.iter().any(|s| s == current)
}

fn evaluate_one(
    model: &TemperModel,
    kind: &InvariantKind,
    required_states: &[String],
    state: &TemperModelState,
) -> bool {
    match kind {
        InvariantKind::StatusInSet => {
            // Status validity is an inherent property of the per-entity
            // model (its transitions only produce declared statuses).
            true
        }
        InvariantKind::CounterPositive { var } => state.counters.get(var).copied().unwrap_or(0) > 0,
        InvariantKind::NeverState { state: forbidden } => state.status != *forbidden,
        InvariantKind::BoolRequired { var, expect } => {
            state.booleans.get(var).copied() == Some(*expect)
        }
        InvariantKind::NoFurtherTransitions => !model.transitions.iter().any(|transition| {
            let status_matches = transition.from_states.is_empty()
                || transition
                    .from_states
                    .iter()
                    .any(|from| from == &state.status);
            status_matches && crate::model::semantics::evaluate_guard(&transition.guard, state)
        }),
        InvariantKind::Implication => {
            required_states.is_empty() || required_states.contains(&state.status)
        }
        InvariantKind::CounterCompare { var, op, value } => {
            let counter = state.counters.get(var).copied().unwrap_or(0);
            match op {
                temper_spec::automaton::AssertCompareOp::Gt => counter > *value,
                temper_spec::automaton::AssertCompareOp::Gte => counter >= *value,
                temper_spec::automaton::AssertCompareOp::Lt => counter < *value,
                temper_spec::automaton::AssertCompareOp::Lte => counter <= *value,
                temper_spec::automaton::AssertCompareOp::Eq => counter == *value,
            }
        }
        InvariantKind::And(kinds) => kinds
            .iter()
            .all(|kind| evaluate_one(model, kind, required_states, state)),
        InvariantKind::Or(kinds) => kinds
            .iter()
            .any(|kind| evaluate_one(model, kind, required_states, state)),
        InvariantKind::Unverifiable { .. } => true, // warning issued elsewhere
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use temper_spec::automaton::parse_automaton;

    fn build(spec: &str) -> TemperModel {
        let aut = parse_automaton(spec).unwrap();
        crate::model::build_model_from_automaton(&aut, 3)
    }

    fn state(status: &str) -> TemperModelState {
        TemperModelState {
            status: status.to_string(),
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
        }
    }

    #[test]
    fn never_state_detects_forbidden_status() {
        let spec = r#"
[automaton]
name = "X"
states = ["A", "Forbidden"]
initial = "A"

[[action]]
name = "GoBad"
from = ["A"]
to = "Forbidden"

[[invariant]]
name = "NoForbidden"
assert = "never(Forbidden)"
"#;
        let model = build(spec);
        assert!(all_local_invariants_hold(&model, &state("A")));
        assert!(!all_local_invariants_hold(&model, &state("Forbidden")));
    }

    #[test]
    fn empty_invariants_pass() {
        let spec = r#"
[automaton]
name = "Y"
states = ["A", "B"]
initial = "A"

[[action]]
name = "Go"
from = ["A"]
to = "B"
"#;
        let model = build(spec);
        assert!(all_local_invariants_hold(&model, &state("A")));
        assert!(all_local_invariants_hold(&model, &state("B")));
    }

    #[test]
    fn counter_and_boolean_invariants_require_real_evidence() {
        let spec = r#"
[automaton]
name = "Evidence"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "attempts"
type = "counter"
initial = "0"

[[state]]
name = "approved"
type = "bool"
initial = "false"

[[invariant]]
name = "Attempted"
when = ["Done"]
assert = "attempts > 0"

[[invariant]]
name = "Approved"
when = ["Done"]
assert = "approved"
"#;
        let model = build(spec);
        let mut done = state("Done");
        assert!(!all_local_invariants_hold(&model, &done));
        done.counters.insert("attempts".to_string(), 1);
        assert!(!all_local_invariants_hold(&model, &done));
        done.booleans.insert("approved".to_string(), true);
        assert!(all_local_invariants_hold(&model, &done));
    }

    #[test]
    fn terminal_invariant_checks_enabled_transitions() {
        let terminal_spec = r#"
[automaton]
name = "Terminal"
states = ["Open", "Done"]
initial = "Open"

[[action]]
name = "Finish"
from = ["Open"]
to = "Done"

[[invariant]]
name = "DoneIsTerminal"
when = ["Done"]
assert = "no_further_transitions"
"#;
        let terminal = build(terminal_spec);
        assert!(all_local_invariants_hold(&terminal, &state("Done")));

        let reopenable_spec = terminal_spec.replace(
            "[[invariant]]",
            "[[action]]\nname = \"Reopen\"\nfrom = [\"Done\"]\nto = \"Open\"\n\n[[invariant]]",
        );
        let reopenable = build(&reopenable_spec);
        assert!(!all_local_invariants_hold(&reopenable, &state("Done")));
    }
}
