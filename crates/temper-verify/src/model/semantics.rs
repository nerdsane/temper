//! Shared concrete guard/effect/invariant semantics for verification backends.

use std::collections::BTreeSet;

use stateright::Model;

use temper_spec::automaton::AssertCompareOp;

use super::types::{InvariantKind, ModelEffect, ModelGuard, TemperModel, TemperModelState};

/// Evaluate a model guard against a concrete model state.
pub fn evaluate_guard(guard: &ModelGuard, state: &TemperModelState) -> bool {
    match guard {
        ModelGuard::Always => true,
        ModelGuard::StateIn(states) => states.iter().any(|s| s == &state.status),
        ModelGuard::CounterMin { var, min } => {
            let val = state.counters.get(var).copied().unwrap_or(0);
            val >= *min
        }
        ModelGuard::CounterMax { var, max } => {
            let val = state.counters.get(var).copied().unwrap_or(0);
            val < *max
        }
        ModelGuard::BoolTrue(var) => state.booleans.get(var).copied().unwrap_or(false),
        ModelGuard::BoolFalse(var) => !state.booleans.get(var).copied().unwrap_or(false),
        ModelGuard::ListContains { var, value } => state
            .lists
            .get(var)
            .is_some_and(|vals| vals.iter().any(|v| v == value)),
        ModelGuard::ListLengthMin { var, min } => state.lists.get(var).map_or(0, Vec::len) >= *min,
        ModelGuard::And(guards) => guards.iter().all(|g| evaluate_guard(g, state)),
    }
}

/// Apply model effects to the provided state.
///
/// `action_name` is used to generate deterministic symbolic list elements.
pub fn apply_effects(effects: &[ModelEffect], state: &mut TemperModelState, action_name: &str) {
    for effect in effects {
        match effect {
            ModelEffect::IncrementCounter(var) => {
                let entry = state.counters.entry(var.clone()).or_insert(0);
                *entry += 1;
            }
            ModelEffect::DecrementCounter(var) => {
                let entry = state.counters.entry(var.clone()).or_insert(0);
                *entry = entry.saturating_sub(1);
            }
            ModelEffect::SetBool { var, value } => {
                state.booleans.insert(var.clone(), *value);
            }
            ModelEffect::ListAppend(var) => {
                let entry = state.lists.entry(var.clone()).or_default();
                let next_idx = entry.len() + 1;
                entry.push(format!("{action_name}#{next_idx}"));
            }
            ModelEffect::ListRemoveAt(var) => {
                if let Some(entry) = state.lists.get_mut(var)
                    && !entry.is_empty()
                {
                    entry.remove(0);
                }
            }
        }
    }
}

/// How [`evaluate_invariant_kind`] decides whether a transition counts as
/// "enabled" when evaluating [`InvariantKind::NoFurtherTransitions`].
///
/// The two semantics genuinely differ at exploration boundaries: a
/// transition whose `IncrementCounter`/`ListAppend` effect would exceed the
/// bounded-exploration budget is excluded by `Model::actions` but still
/// counts as enabled under a status+guard-only scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoFurtherTransitionsMode {
    /// A transition is enabled iff `Model::actions` would emit it
    /// (status precondition + guard + bounded-exploration counter/list
    /// budgets). Used by the deterministic-simulation and property-test
    /// invariant checkers.
    EnabledActions,
    /// A transition is enabled iff its status precondition and guard hold;
    /// bounded-exploration budgets are ignored. Used by the Stateright
    /// property functions, which replicate the status+guard scan without
    /// the budget filter.
    GuardOnly,
}

/// Evaluate a single [`InvariantKind`] against a concrete model state.
/// Returns `true` when the invariant HOLDS.
///
/// This is the single authoritative invariant-kind evaluator shared by the
/// Stateright compound-property checker, the deterministic-simulation
/// checker, the property-test checker, and (for the kinds it projects) the
/// composite verifier. Callers that need the "violated" form negate the
/// result.
///
/// Pure recursion over compound variants; does not consult
/// `trigger_states` — callers gate on triggers before calling this.
pub fn evaluate_invariant_kind(
    kind: &InvariantKind,
    required_states: &[String],
    model: &TemperModel,
    state: &TemperModelState,
    nft_mode: NoFurtherTransitionsMode,
) -> bool {
    match kind {
        InvariantKind::StatusInSet => model.states.contains(&state.status),
        InvariantKind::CounterPositive { var } => state.counters.get(var).copied().unwrap_or(0) > 0,
        InvariantKind::BoolRequired { var, expect } => {
            state.booleans.get(var).copied().unwrap_or(false) == *expect
        }
        InvariantKind::NoFurtherTransitions => match nft_mode {
            NoFurtherTransitionsMode::EnabledActions => {
                let mut actions = Vec::new();
                model.actions(state, &mut actions);
                actions.is_empty()
            }
            NoFurtherTransitionsMode::GuardOnly => !model.transitions.iter().any(|t| {
                let status_ok =
                    t.from_states.is_empty() || t.from_states.iter().any(|s| s == &state.status);
                status_ok && evaluate_guard(&t.guard, state)
            }),
        },
        InvariantKind::Implication => {
            let valid_required: Vec<&String> = required_states
                .iter()
                .filter(|s| model.states.contains(s))
                .collect();
            // No valid required states: trivially true (the invariant
            // constrains non-status variables).
            valid_required.is_empty() || valid_required.contains(&&state.status)
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
            .all(|k| evaluate_invariant_kind(k, required_states, model, state, nft_mode)),
        InvariantKind::Or(parts) => parts
            .iter()
            .any(|k| evaluate_invariant_kind(k, required_states, model, state, nft_mode)),
        InvariantKind::Unverifiable { .. } => true,
    }
}

/// Collect all `(list_var, value)` pairs referenced by `ListContains` guards.
pub fn collect_list_contains_pairs(guard: &ModelGuard, pairs: &mut BTreeSet<(String, String)>) {
    match guard {
        ModelGuard::ListContains { var, value } => {
            pairs.insert((var.clone(), value.clone()));
        }
        ModelGuard::And(guards) => {
            for g in guards {
                collect_list_contains_pairs(g, pairs);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "semantics_test.rs"]
mod tests;
