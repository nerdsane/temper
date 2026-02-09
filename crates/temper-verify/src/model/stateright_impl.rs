//! Stateright `Model` implementation for `TemperModel`.
//!
//! Implements `init_states`, `actions`, `next_state`, and `properties` to
//! enable exhaustive model checking via Stateright. Supports multi-variable
//! state (counters + booleans), safety invariants, and liveness properties.

use stateright::{Model, Property};

use super::types::{
    InvariantKind, LivenessKind, ModelEffect, ModelGuard, TemperModel, TemperModelAction,
    TemperModelState,
};

// -- Guard evaluation --------------------------------------------------------

/// Evaluate a model guard against the current state.
fn evaluate_guard(guard: &ModelGuard, state: &TemperModelState) -> bool {
    match guard {
        ModelGuard::Always => true,
        ModelGuard::CounterMin { var, min } => {
            let val = state.counters.get(var).copied().unwrap_or(0);
            val >= *min
        }
        ModelGuard::BoolTrue(var) => state.booleans.get(var).copied().unwrap_or(false),
        ModelGuard::And(guards) => guards.iter().all(|g| evaluate_guard(g, state)),
    }
}

// -- Property condition functions (bare fn pointers) -------------------------

/// Check that the current status is in the set of valid states (TypeInvariant).
fn check_status_in_set(model: &TemperModel, state: &TemperModelState) -> bool {
    model.states.contains(&state.status)
}

/// Check all CounterPositive invariants: when status is in triggers, counter > 0.
fn check_counter_positive(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if let InvariantKind::CounterPositive { ref var } = inv.kind {
            let triggered = inv.trigger_states.is_empty()
                || inv.trigger_states.contains(&state.status);
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

/// Check all BoolRequired invariants: when status is in triggers, bool must be true.
fn check_bool_required(model: &TemperModel, state: &TemperModelState) -> bool {
    for inv in &model.invariants {
        if let InvariantKind::BoolRequired { ref var } = inv.kind {
            let triggered = inv.trigger_states.is_empty()
                || inv.trigger_states.contains(&state.status);
            if triggered {
                let val = state.booleans.get(var).copied().unwrap_or(false);
                if !val {
                    return false;
                }
            }
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
        let triggered = inv.trigger_states.is_empty()
            || inv.trigger_states.contains(&state.status);
        if triggered {
            // Check that no transitions are enabled from this state
            let mut actions = Vec::new();
            // We need to check actions manually since we can't call model.actions()
            // inside a property fn (it would recurse). Instead, replicate the logic.
            for t in &model.transitions {
                let status_ok = t.from_states.is_empty()
                    || t.from_states.iter().any(|s| s == &state.status);
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

// -- Liveness property functions ---------------------------------------------

/// Check liveness: from the specified states, at least one action is enabled.
/// (Deadlock freedom expressed as a safety property.)
fn check_no_deadlock(model: &TemperModel, state: &TemperModelState) -> bool {
    for live in &model.liveness {
        if let LivenessKind::NoDeadlock { ref from } = live.kind {
            if from.contains(&state.status) {
                // Must have at least one enabled action
                let mut has_action = false;
                for t in &model.transitions {
                    let status_ok = t.from_states.is_empty()
                        || t.from_states.iter().any(|s| s == &state.status);
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
    }
    true
}

/// Check liveness: from the specified states, eventually reaches a target state.
/// Uses Stateright's `Property::eventually` (acyclic paths only).
fn check_reaches_state(model: &TemperModel, state: &TemperModelState) -> bool {
    for live in &model.liveness {
        if let LivenessKind::ReachesState {
            ref from,
            ref targets,
        } = live.kind
        {
            if targets.is_empty() {
                continue; // No targets = trivially true
            }
            // The eventually check: if we're in a from-state OR a non-target state,
            // this property hasn't been satisfied yet. It's satisfied when we reach
            // a target state.
            // Stateright's eventually semantics: the condition must become true at
            // some point on every acyclic path.
            if from.contains(&state.status) || !targets.contains(&state.status) {
                // Not yet at target — Stateright handles the path analysis
                // We return false to indicate "not yet satisfied"
                // But we need to be careful: we return true if we ARE at a target
            }
            if targets.contains(&state.status) {
                return true;
            }
        }
    }
    // If no liveness reaches apply, or we haven't reached target yet
    // For eventually properties, Stateright needs: return true when the property
    // is "satisfied at this state". For ReachesState, that means we're at a target.
    // If no ReachesState liveness exists, return true (vacuously satisfied).
    let has_reaches = model.liveness.iter().any(|l| matches!(l.kind, LivenessKind::ReachesState { .. }));
    !has_reaches
}

// -- Model trait implementation ----------------------------------------------

impl Model for TemperModel {
    type State = TemperModelState;
    type Action = TemperModelAction;

    fn init_states(&self) -> Vec<Self::State> {
        vec![TemperModelState {
            status: self.initial_status.clone(),
            counters: self.initial_counters.clone(),
            booleans: self.initial_booleans.clone(),
        }]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        for t in &self.transitions {
            // Check status precondition
            let status_ok = t.from_states.is_empty()
                || t.from_states.iter().any(|s| s == &state.status);
            if !status_ok {
                continue;
            }

            // Check guard
            if !evaluate_guard(&t.guard, state) {
                continue;
            }

            // Check counter bounds: increment effects must not exceed bounds
            let mut within_bounds = true;
            for effect in &t.effects {
                if let ModelEffect::IncrementCounter(var) = effect {
                    let current = state.counters.get(var).copied().unwrap_or(0);
                    let bound = self
                        .counter_bounds
                        .get(var)
                        .copied()
                        .unwrap_or(self.default_max_counter);
                    if current >= bound {
                        within_bounds = false;
                        break;
                    }
                }
                if let ModelEffect::DecrementCounter(var) = effect {
                    let current = state.counters.get(var).copied().unwrap_or(0);
                    if current == 0 {
                        within_bounds = false;
                        break;
                    }
                }
            }
            if !within_bounds {
                continue;
            }

            actions.push(TemperModelAction {
                name: t.name.clone(),
                target_state: t.to_state.clone(),
            });
        }
    }

    fn next_state(&self, state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let resolved = self.transitions.iter().find(|t| t.name == action.name)?;

        let new_status = action
            .target_state
            .unwrap_or_else(|| state.status.clone());

        let mut new_counters = state.counters.clone();
        let mut new_booleans = state.booleans.clone();

        for effect in &resolved.effects {
            match effect {
                ModelEffect::IncrementCounter(var) => {
                    let entry = new_counters.entry(var.clone()).or_insert(0);
                    *entry += 1;
                }
                ModelEffect::DecrementCounter(var) => {
                    let entry = new_counters.entry(var.clone()).or_insert(0);
                    *entry = entry.saturating_sub(1);
                }
                ModelEffect::SetBool { var, value } => {
                    new_booleans.insert(var.clone(), *value);
                }
            }
        }

        Some(TemperModelState {
            status: new_status,
            counters: new_counters,
            booleans: new_booleans,
        })
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
            .any(|l| matches!(l.kind, LivenessKind::ReachesState { .. }));
        if has_reaches {
            props.push(Property::eventually(
                "ReachesTerminal",
                check_reaches_state,
            ));
        }

        props
    }
}
