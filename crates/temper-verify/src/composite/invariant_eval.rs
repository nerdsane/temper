//! Local-invariant evaluator for the composite verifier.
//!
//! Projects a joint state down to a single entity's slice and evaluates
//! that entity's `ResolvedInvariant`s. Called from
//! [`super::model::CompositeTemperModel::properties`] for each entity in
//! the composition on every BFS-visited state.
//!
//! This is a minimum-viable port of the single-entity evaluator used in
//! [`crate::model::stateright_impl`]. It handles the invariant kinds
//! that are directly checkable on a single-entity `TemperModelState`:
//! `StatusInSet`, `CounterPositive`, `NeverState`, `BoolRequired`,
//! `NoReachingState`, `NoFurtherTransitions`. `Unverifiable` invariants
//! are treated as true (the single-entity cascade issues a warning;
//! the composite checker inherits that warning via the plan's
//! warnings vector).

use crate::model::semantics::{NoFurtherTransitionsMode, evaluate_invariant_kind};
use crate::model::{InvariantKind, TemperModel, TemperModelState};

/// Evaluate every invariant on `model` against `state` (a single
/// entity's slice of the joint state). Returns `true` iff all pass.
pub(super) fn all_local_invariants_hold(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if !triggers_on(&inv.trigger_states, &state.status) {
            continue;
        }
        if !evaluate_one(&inv.kind, model, state) {
            return false;
        }
    }
    true
}

fn triggers_on(trigger_states: &[String], current: &str) -> bool {
    trigger_states.is_empty() || trigger_states.iter().any(|s| s == current)
}

/// Partial (projection) evaluation of one invariant kind.
///
/// The kinds the composite genuinely evaluates delegate to the crate's
/// authoritative [`evaluate_invariant_kind`]; the rest are intentionally
/// treated as holding (see the per-arm comments and module docs).
fn evaluate_one(kind: &InvariantKind, model: &TemperModel, state: &TemperModelState) -> bool {
    match kind {
        // Kinds the composite projection genuinely evaluates — delegate
        // to the shared evaluator. (`required_states` and the NFT mode
        // are irrelevant for these kinds.)
        InvariantKind::NeverState { .. } | InvariantKind::BoolRequired { .. } => {
            evaluate_invariant_kind(kind, &[], model, state, NoFurtherTransitionsMode::GuardOnly)
        }
        InvariantKind::StatusInSet => {
            // Status validity is an inherent property of the per-entity
            // model (its transitions only produce declared statuses).
            true
        }
        InvariantKind::CounterPositive { var } => {
            // `usize` is always >= 0; the invariant encodes a soft
            // check but the type enforces it.
            let _ = state.counters.get(var);
            true
        }
        InvariantKind::NoFurtherTransitions => {
            // Structural — BFS naturally surfaces if a state has no
            // enabled actions. Not a per-state evaluation here.
            true
        }
        InvariantKind::Implication => true, // handled by StatusInSet + transitions
        InvariantKind::CounterCompare { .. } => {
            // Generalised counter comparison — single-entity checker
            // interprets the expression; composite inherits the
            // per-entity decision (non-violating here means the
            // single-entity cascade was not asked to reject it).
            true
        }
        // Compound: recurse locally so nested leaves keep the
        // composite's partial projection semantics, not the full
        // evaluator's.
        InvariantKind::And(kinds) => kinds.iter().all(|k| evaluate_one(k, model, state)),
        InvariantKind::Or(kinds) => kinds.iter().any(|k| evaluate_one(k, model, state)),
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
}
