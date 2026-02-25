//! Shared concrete guard/effect semantics for verification backends.

use std::collections::BTreeSet;

use super::types::{ModelEffect, ModelGuard, TemperModelState};

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
