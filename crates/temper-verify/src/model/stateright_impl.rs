//! Stateright `Model` implementation for `TemperModel`.
//!
//! Implements `init_states`, `actions`, `next_state`, and `properties` to
//! enable exhaustive model checking via Stateright. Supports multi-variable
//! state (counters + booleans), safety invariants, and liveness properties.

use stateright::{Model, Property};

use super::reference_contract::apply_effects as apply_reference_effects;
use super::semantics::{apply_effects, evaluate_guard};
use temper_spec::automaton::AssertCompareOp;

use super::types::{InvariantKind, LivenessKind, TemperModel, TemperModelAction, TemperModelState};

// -- Property condition functions (bare fn pointers) -------------------------

/// Check that the current status is in the set of valid states (TypeInvariant).
fn check_status_in_set(model: &TemperModel, state: &TemperModelState) -> bool {
    model.states.contains(&state.status)
}

/// Check all CounterPositive invariants: when status is in triggers, counter > 0.
fn check_counter_positive(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if let InvariantKind::CounterPositive { ref var } = inv.kind {
            let triggered =
                inv.trigger_states.is_empty() || inv.trigger_states.contains(&state.status);
            if triggered {
                let val = state.counters.get(var).copied().unwrap_or(0);
                if val == 0 {
                    return false;
                }
            }
        }
    }
    true
}

/// Check all BoolRequired invariants: when status is in triggers, bool matches `expect`.
fn check_bool_required(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if let InvariantKind::BoolRequired { ref var, expect } = inv.kind {
            let triggered =
                inv.trigger_states.is_empty() || inv.trigger_states.contains(&state.status);
            if triggered {
                let val = state.booleans.get(var).copied().unwrap_or(false);
                if val != expect {
                    return false;
                }
            }
        }
    }
    true
}

/// Evaluate a single [`InvariantKind`] against state. Returns `true` if holds.
///
/// Shared by `check_compound_invariants` for `And`/`Or` recursion.
fn kind_holds(
    kind: &InvariantKind,
    required_states: &[String],
    model: &TemperModel,
    state: &TemperModelState,
) -> bool {
    match kind {
        InvariantKind::StatusInSet => model.states.contains(&state.status),
        InvariantKind::CounterPositive { var } => state.counters.get(var).copied().unwrap_or(0) > 0,
        InvariantKind::BoolRequired { var, expect } => {
            state.booleans.get(var).copied().unwrap_or(false) == *expect
        }
        InvariantKind::NoFurtherTransitions => {
            // Holds iff no transitions are enabled from current state.
            !model.transitions.iter().any(|t| {
                let status_ok =
                    t.from_states.is_empty() || t.from_states.iter().any(|s| s == &state.status);
                status_ok && evaluate_guard(&t.guard, state)
            })
        }
        InvariantKind::Implication => {
            let valid: Vec<&String> = required_states
                .iter()
                .filter(|s| model.states.contains(s))
                .collect();
            valid.is_empty() || valid.contains(&&state.status)
        }
        InvariantKind::CounterCompare { var, op, value } => {
            let val = state.counters.get(var).copied().unwrap_or(0);
            match op {
                AssertCompareOp::Gt => val > *value,
                AssertCompareOp::Gte => val >= *value,
                AssertCompareOp::Lt => val < *value,
                AssertCompareOp::Lte => val <= *value,
                AssertCompareOp::Eq => val == *value,
            }
        }
        InvariantKind::NeverState { state: forbidden } => state.status != *forbidden,
        InvariantKind::And(parts) => parts
            .iter()
            .all(|k| kind_holds(k, required_states, model, state)),
        InvariantKind::Or(parts) => parts
            .iter()
            .any(|k| kind_holds(k, required_states, model, state)),
        InvariantKind::Unverifiable { .. } => true,
    }
}

/// Check all compound (And/Or) invariants via recursive `kind_holds`.
fn check_compound_invariants(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if !matches!(inv.kind, InvariantKind::And(_) | InvariantKind::Or(_)) {
            continue;
        }
        let triggered = inv.trigger_states.is_empty() || inv.trigger_states.contains(&state.status);
        if triggered && !kind_holds(&inv.kind, &inv.required_states, model, state) {
            return false;
        }
    }
    true
}

/// Check all NoFurtherTransitions invariants: when status is in triggers,
/// no actions should be enabled.
fn check_no_further_transitions(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if !matches!(inv.kind, InvariantKind::NoFurtherTransitions) {
            continue;
        }
        let triggered = inv.trigger_states.is_empty() || inv.trigger_states.contains(&state.status);
        if triggered {
            // Check that no transitions are enabled from this state
            let mut actions = Vec::new();
            // We need to check actions manually since we can't call model.actions()
            // inside a property fn (it would recurse). Instead, replicate the logic.
            for t in &model.transitions {
                let status_ok =
                    t.from_states.is_empty() || t.from_states.iter().any(|s| s == &state.status);
                if status_ok && evaluate_guard(&t.guard, state) {
                    actions.push(&t.name);
                }
            }
            if !actions.is_empty() {
                return false;
            }
        }
    }
    true
}

/// Check all implication invariants: when status is in trigger_states,
/// it must also be in required_states.
fn check_implications(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if !matches!(inv.kind, InvariantKind::Implication) {
            continue;
        }
        if inv.trigger_states.contains(&state.status) {
            let valid_required: Vec<&String> = inv
                .required_states
                .iter()
                .filter(|s| model.states.contains(s))
                .collect();

            if valid_required.is_empty() {
                continue; // Trivially true (constrains non-status variables)
            }
            if !valid_required.contains(&&state.status) {
                return false;
            }
        }
    }
    true
}

/// Check all CounterCompare invariants: when status is in triggers, counter op value.
fn check_counter_compare(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if let InvariantKind::CounterCompare {
            ref var,
            ref op,
            value,
        } = inv.kind
        {
            let triggered =
                inv.trigger_states.is_empty() || inv.trigger_states.contains(&state.status);
            if triggered {
                let val = state.counters.get(var).copied().unwrap_or(0);
                let holds = match op {
                    AssertCompareOp::Gt => val > value,
                    AssertCompareOp::Gte => val >= value,
                    AssertCompareOp::Lt => val < value,
                    AssertCompareOp::Lte => val <= value,
                    AssertCompareOp::Eq => val == value,
                };
                if !holds {
                    return false;
                }
            }
        }
    }
    true
}

/// Check all NeverState invariants: entity should never be in the forbidden state.
fn check_never_state(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if let InvariantKind::NeverState { state: forbidden } = &inv.kind
            && state.status == *forbidden
        {
            return false;
        }
    }
    true
}

// -- Liveness property functions ---------------------------------------------

/// Check liveness: from the specified states, at least one action is enabled.
/// (Deadlock freedom expressed as a safety property.)
fn check_no_deadlock(model: &TemperModel, state: &TemperModelState) -> bool {
    for live in &model.liveness {
        if let LivenessKind::NoDeadlock { ref from } = live.kind
            && from.contains(&state.status)
        {
            // Must have at least one enabled action
            let mut has_action = false;
            for t in &model.transitions {
                let status_ok =
                    t.from_states.is_empty() || t.from_states.iter().any(|s| s == &state.status);
                if status_ok && evaluate_guard(&t.guard, state) {
                    has_action = true;
                    break;
                }
            }
            if !has_action {
                return false;
            }
        }
    }
    true
}

/// Check liveness: from the specified states, eventually reaches a target state.
///
/// Returns `true` when the current state is in any ReachesState target set.
/// Stateright's `eventually` verifies that on every acyclic path, this
/// predicate becomes true at some point.
///
/// Note: Stateright requires `fn` pointers, so we combine all ReachesState
/// properties. For specs with multiple ReachesState targets, "eventually
/// reaches any target" is verified.
fn check_reaches_state(model: &TemperModel, state: &TemperModelState) -> bool {
    for live in &model.liveness {
        if let LivenessKind::ReachesState { targets, .. } = &live.kind
            && !targets.is_empty()
            && targets.contains(&state.status)
        {
            return true;
        }
    }
    // No target state reached yet.
    // If there are no ReachesState properties, return true (vacuously satisfied).
    !model.liveness.iter().any(
        |l| matches!(&l.kind, LivenessKind::ReachesState { targets, .. } if !targets.is_empty()),
    )
}

// -- Model trait implementation ----------------------------------------------

impl Model for TemperModel {
    type State = TemperModelState;
    type Action = TemperModelAction;

    fn init_states(&self) -> Vec<Self::State> {
        let variants = if self.initial_counter_variants.is_empty() {
            vec![self.initial_counters.clone()]
        } else {
            self.initial_counter_variants.clone()
        };
        variants
            .into_iter()
            .map(|counters| TemperModelState {
                status: self.initial_status.clone(),
                counters,
                booleans: self.initial_booleans.clone(),
                lists: self.initial_lists.clone(),
            })
            .collect()
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        super::action_enumeration::enumerate_actions(self, state, None, actions);
    }
    fn next_state(&self, state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let resolved = self.transitions.iter().find(|t| t.name == action.name)?;

        let new_status = action.target_state.unwrap_or_else(|| state.status.clone());
        let mut next = state.clone();
        next.status = new_status;
        apply_effects(&resolved.effects, &mut next, &action.name);
        if !apply_reference_effects(&resolved.effects, &mut next, &action.reference_params) {
            return None;
        }
        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        let mut props = Vec::new();

        // Safety: TypeInvariant (always included)
        let has_status_check = self
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::StatusInSet));
        if has_status_check {
            props.push(Property::always("TypeInvariant", check_status_in_set));
        }

        // Safety: CounterPositive invariants
        let has_counter_check = self
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::CounterPositive { .. }));
        if has_counter_check {
            props.push(Property::always(
                "CounterPositiveInvariants",
                check_counter_positive,
            ));
        }

        // Safety: BoolRequired invariants
        let has_bool_check = self
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::BoolRequired { .. }));
        if has_bool_check {
            props.push(Property::always(
                "BoolRequiredInvariants",
                check_bool_required,
            ));
        }

        // Safety: NoFurtherTransitions invariants
        let has_nft = self
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::NoFurtherTransitions));
        if has_nft {
            props.push(Property::always(
                "NoFurtherTransitions",
                check_no_further_transitions,
            ));
        }

        // Safety: Implication invariants
        let has_implication = self
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::Implication));
        if has_implication {
            props.push(Property::always(
                "ImplicationInvariants",
                check_implications,
            ));
        }

        // Safety: CounterCompare invariants
        let has_counter_compare = self
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::CounterCompare { .. }));
        if has_counter_compare {
            props.push(Property::always(
                "CounterCompareInvariants",
                check_counter_compare,
            ));
        }

        // Safety: NeverState invariants
        let has_never_state = self
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::NeverState { .. }));
        if has_never_state {
            props.push(Property::always("NeverStateInvariants", check_never_state));
        }

        // Safety: Compound (And/Or) invariants
        let has_compound = self
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::And(_) | InvariantKind::Or(_)));
        if has_compound {
            props.push(Property::always(
                "CompoundInvariants",
                check_compound_invariants,
            ));
        }

        // Note: Unverifiable invariants generate no properties (skipped).

        // Liveness: NoDeadlock (expressed as safety: "always has actions")
        let has_no_deadlock = self
            .liveness
            .iter()
            .any(|l| matches!(l.kind, LivenessKind::NoDeadlock { .. }));
        if has_no_deadlock {
            props.push(Property::always("NoDeadlock", check_no_deadlock));
        }

        // Liveness: ReachesState (Stateright's eventually — acyclic paths only)
        let has_reaches = self
            .liveness
            .iter()
            .any(|l| matches!(&l.kind, LivenessKind::ReachesState { targets, .. } if !targets.is_empty()));
        if has_reaches {
            props.push(Property::eventually("ReachesTerminal", check_reaches_state));
        }

        props
    }
}
