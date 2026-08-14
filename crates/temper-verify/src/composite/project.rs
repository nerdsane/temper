//! Join-vector projection for composite verification.
//!
//! Per-entity cascade already proved local bools, counters, and lists.
//! The joint checker only needs the fields other entities actually read.
//! Today that is `status` ([`ModelGuard::CrossEntityState`]). Projecting
//! each slice to status keeps the join tractable (DesignLanguage's 800k
//! local combinations do not multiply the actor graph) without dropping
//! cross-entity guards.

use crate::model::{InvariantKind, ModelEffect, ModelGuard, TemperModel};

/// Collapse `model` to the composite join vector: `status` plus any
/// cross-entity guard. Local bool/counter/list state, effects, and
/// invariants are stripped; those local guards become `Always` so
/// status transitions stay reachable.
pub fn project_model_to_join_vector(model: &mut TemperModel) {
    model.initial_counters.clear();
    model.initial_booleans.clear();
    model.initial_lists.clear();
    model.counter_bounds.clear();

    for transition in &mut model.transitions {
        transition.guard = project_guard(&transition.guard);
        transition.effects.retain(effect_on_join_vector);
    }

    model
        .invariants
        .retain(|inv| invariant_on_join_vector(&inv.kind));
}

fn project_guard(guard: &ModelGuard) -> ModelGuard {
    match guard {
        ModelGuard::Always | ModelGuard::StateIn(_) | ModelGuard::CrossEntityState { .. } => {
            guard.clone()
        }
        ModelGuard::BoolTrue(_)
        | ModelGuard::BoolFalse(_)
        | ModelGuard::CounterMin { .. }
        | ModelGuard::CounterMax { .. }
        | ModelGuard::ListContains { .. }
        | ModelGuard::ListLengthMin { .. } => ModelGuard::Always,
        ModelGuard::And(guards) => {
            let mut kept: Vec<ModelGuard> = guards
                .iter()
                .map(project_guard)
                .filter(|g| !matches!(g, ModelGuard::Always))
                .collect();
            match kept.len() {
                0 => ModelGuard::Always,
                1 => kept.pop().expect("len == 1"),
                _ => ModelGuard::And(kept),
            }
        }
    }
}

/// No local effect currently writes a join-vector field (status is
/// set by the transition's `to`, not by an effect).
fn effect_on_join_vector(_effect: &ModelEffect) -> bool {
    false
}

fn invariant_on_join_vector(kind: &InvariantKind) -> bool {
    match kind {
        InvariantKind::StatusInSet
        | InvariantKind::NeverState { .. }
        | InvariantKind::Implication
        | InvariantKind::NoFurtherTransitions
        | InvariantKind::Unverifiable { .. } => true,
        InvariantKind::CounterPositive { .. }
        | InvariantKind::BoolRequired { .. }
        | InvariantKind::CounterCompare { .. } => false,
        InvariantKind::And(kinds) | InvariantKind::Or(kinds) => {
            kinds.iter().all(invariant_on_join_vector)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::automaton::parse_automaton;

    #[test]
    fn strips_bools_and_keeps_status_transition() {
        let spec = r#"
[automaton]
name = "Lang"
states = ["Draft", "Published"]
initial = "Draft"

[[state]]
name = "ready"
type = "bool"
initial = "false"

[[action]]
name = "Mark"
from = ["Draft"]
to = "Draft"
effect = "set ready true"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
guard = [{ type = "is_true", var = "ready" }]
"#;
        let aut = parse_automaton(spec).unwrap();
        let mut model = crate::model::build_model_from_automaton(&aut, 3);
        assert!(!model.initial_booleans.is_empty());
        project_model_to_join_vector(&mut model);
        assert!(model.initial_booleans.is_empty());
        assert!(model.initial_counters.is_empty());
        let publish = model
            .transitions
            .iter()
            .find(|t| t.name == "Publish")
            .expect("Publish");
        assert!(
            !guard_mentions_bool(&publish.guard),
            "local bool must be projected away: {:?}",
            publish.guard
        );
        assert!(publish.effects.is_empty());
        let mark = model
            .transitions
            .iter()
            .find(|t| t.name == "Mark")
            .expect("Mark");
        assert!(mark.effects.is_empty());
    }

    #[test]
    fn keeps_cross_entity_status_guard() {
        let spec = r#"
[automaton]
name = "Actor"
states = ["Idle", "Done"]
initial = "Idle"

[[action]]
name = "Finish"
from = ["Idle"]
to = "Done"
guard = [
  { type = "cross_entity_state", entity_type = "Language", entity_id_source = "language_id", required_status = ["Published"] },
]
"#;
        let aut = parse_automaton(spec).unwrap();
        let mut model = crate::model::build_model_from_automaton(&aut, 3);
        project_model_to_join_vector(&mut model);
        let finish = model
            .transitions
            .iter()
            .find(|t| t.name == "Finish")
            .expect("Finish");
        assert!(
            guard_has_cross_entity(&finish.guard, "Language", "Published"),
            "cross-entity status guard must survive projection: {:?}",
            finish.guard
        );
    }

    fn guard_mentions_bool(guard: &ModelGuard) -> bool {
        match guard {
            ModelGuard::BoolTrue(_) | ModelGuard::BoolFalse(_) => true,
            ModelGuard::And(gs) => gs.iter().any(guard_mentions_bool),
            _ => false,
        }
    }

    fn guard_has_cross_entity(guard: &ModelGuard, entity: &str, status: &str) -> bool {
        match guard {
            ModelGuard::CrossEntityState {
                entity_type,
                required_status,
                ..
            } => entity_type == entity && required_status.iter().any(|s| s == status),
            ModelGuard::And(gs) => gs.iter().any(|g| guard_has_cross_entity(g, entity, status)),
            _ => false,
        }
    }
}
