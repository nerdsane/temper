//! Local-invariant evaluator for the composite verifier.
//!
//! Projects a joint state down to a single entity's slice and evaluates
//! that entity's `ResolvedInvariant`s with the shared evaluator. Called from
//! [`super::model::CompositeTemperModel::properties`] for each entity in
//! the composition on every BFS-visited state. Values the model does not
//! track read as unknown and are not violations (the single-entity cascade
//! warns about them).

use temper_spec::predicate::Truth;

use crate::model::semantics::truth;
use crate::model::{TemperModel, TemperModelState};

/// Evaluate every invariant on `model` against `state` (a single
/// entity's slice of the joint state). Returns `true` iff none is false.
pub(super) fn all_local_invariants_hold(model: &TemperModel, state: &TemperModelState) -> bool {
    model
        .invariants
        .iter()
        .all(|inv| truth(&inv.assert, &model.var_kinds, state) != Truth::False)
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
assert = "status != 'Forbidden'"
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
