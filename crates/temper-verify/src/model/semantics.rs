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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn state(status: &str) -> TemperModelState {
        TemperModelState {
            status: status.to_string(),
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
        }
    }

    #[test]
    fn guard_always_passes() {
        assert!(evaluate_guard(&ModelGuard::Always, &state("Any")));
    }

    #[test]
    fn guard_state_in_matches() {
        let g = ModelGuard::StateIn(vec!["Draft".into(), "Active".into()]);
        assert!(evaluate_guard(&g, &state("Draft")));
        assert!(evaluate_guard(&g, &state("Active")));
        assert!(!evaluate_guard(&g, &state("Closed")));
    }

    #[test]
    fn guard_counter_min() {
        let g = ModelGuard::CounterMin {
            var: "items".into(),
            min: 2,
        };
        let mut s = state("A");
        assert!(!evaluate_guard(&g, &s)); // missing counter defaults to 0
        s.counters.insert("items".into(), 1);
        assert!(!evaluate_guard(&g, &s));
        s.counters.insert("items".into(), 2);
        assert!(evaluate_guard(&g, &s));
        s.counters.insert("items".into(), 5);
        assert!(evaluate_guard(&g, &s));
    }

    #[test]
    fn guard_counter_max() {
        let g = ModelGuard::CounterMax {
            var: "items".into(),
            max: 3,
        };
        let mut s = state("A");
        assert!(evaluate_guard(&g, &s)); // 0 < 3
        s.counters.insert("items".into(), 2);
        assert!(evaluate_guard(&g, &s)); // 2 < 3
        s.counters.insert("items".into(), 3);
        assert!(!evaluate_guard(&g, &s)); // 3 < 3 is false
    }

    #[test]
    fn guard_bool_true() {
        let g = ModelGuard::BoolTrue("ready".into());
        let mut s = state("A");
        assert!(!evaluate_guard(&g, &s)); // missing defaults to false
        s.booleans.insert("ready".into(), false);
        assert!(!evaluate_guard(&g, &s));
        s.booleans.insert("ready".into(), true);
        assert!(evaluate_guard(&g, &s));
    }

    #[test]
    fn guard_list_contains() {
        let g = ModelGuard::ListContains {
            var: "tags".into(),
            value: "vip".into(),
        };
        let mut s = state("A");
        assert!(!evaluate_guard(&g, &s)); // no list
        s.lists.insert("tags".into(), vec!["basic".into()]);
        assert!(!evaluate_guard(&g, &s));
        s.lists
            .insert("tags".into(), vec!["vip".into(), "basic".into()]);
        assert!(evaluate_guard(&g, &s));
    }

    #[test]
    fn guard_list_length_min() {
        let g = ModelGuard::ListLengthMin {
            var: "items".into(),
            min: 2,
        };
        let mut s = state("A");
        assert!(!evaluate_guard(&g, &s));
        s.lists.insert("items".into(), vec!["a".into()]);
        assert!(!evaluate_guard(&g, &s));
        s.lists.insert("items".into(), vec!["a".into(), "b".into()]);
        assert!(evaluate_guard(&g, &s));
    }

    #[test]
    fn guard_and_all_must_pass() {
        let g = ModelGuard::And(vec![
            ModelGuard::StateIn(vec!["Draft".into()]),
            ModelGuard::CounterMin {
                var: "items".into(),
                min: 1,
            },
        ]);
        let mut s = state("Draft");
        assert!(!evaluate_guard(&g, &s)); // counter fails
        s.counters.insert("items".into(), 1);
        assert!(evaluate_guard(&g, &s));
        s.status = "Active".into();
        assert!(!evaluate_guard(&g, &s)); // state fails
    }

    #[test]
    fn effect_increment_counter() {
        let mut s = state("A");
        apply_effects(&[ModelEffect::IncrementCounter("x".into())], &mut s, "Act");
        assert_eq!(s.counters["x"], 1);
        apply_effects(&[ModelEffect::IncrementCounter("x".into())], &mut s, "Act");
        assert_eq!(s.counters["x"], 2);
    }

    #[test]
    fn effect_decrement_counter_saturates_at_zero() {
        let mut s = state("A");
        apply_effects(&[ModelEffect::DecrementCounter("x".into())], &mut s, "Act");
        assert_eq!(s.counters["x"], 0); // saturating sub from 0
        s.counters.insert("x".into(), 3);
        apply_effects(&[ModelEffect::DecrementCounter("x".into())], &mut s, "Act");
        assert_eq!(s.counters["x"], 2);
    }

    #[test]
    fn effect_set_bool() {
        let mut s = state("A");
        apply_effects(
            &[ModelEffect::SetBool {
                var: "done".into(),
                value: true,
            }],
            &mut s,
            "Act",
        );
        assert!(s.booleans["done"]);
        apply_effects(
            &[ModelEffect::SetBool {
                var: "done".into(),
                value: false,
            }],
            &mut s,
            "Act",
        );
        assert!(!s.booleans["done"]);
    }

    #[test]
    fn effect_list_append() {
        let mut s = state("A");
        apply_effects(&[ModelEffect::ListAppend("log".into())], &mut s, "AddItem");
        assert_eq!(s.lists["log"], vec!["AddItem#1"]);
        apply_effects(&[ModelEffect::ListAppend("log".into())], &mut s, "AddItem");
        assert_eq!(s.lists["log"], vec!["AddItem#1", "AddItem#2"]);
    }

    #[test]
    fn effect_list_remove_at() {
        let mut s = state("A");
        s.lists
            .insert("log".into(), vec!["a".into(), "b".into(), "c".into()]);
        apply_effects(&[ModelEffect::ListRemoveAt("log".into())], &mut s, "Act");
        assert_eq!(s.lists["log"], vec!["b", "c"]);
    }

    #[test]
    fn effect_list_remove_at_empty_is_noop() {
        let mut s = state("A");
        s.lists.insert("log".into(), vec![]);
        apply_effects(&[ModelEffect::ListRemoveAt("log".into())], &mut s, "Act");
        assert!(s.lists["log"].is_empty());
    }

    #[test]
    fn collect_list_contains_from_nested_guard() {
        let guard = ModelGuard::And(vec![
            ModelGuard::ListContains {
                var: "tags".into(),
                value: "vip".into(),
            },
            ModelGuard::And(vec![
                ModelGuard::ListContains {
                    var: "roles".into(),
                    value: "admin".into(),
                },
                ModelGuard::Always,
            ]),
            ModelGuard::CounterMin {
                var: "x".into(),
                min: 1,
            },
        ]);
        let mut pairs = BTreeSet::new();
        collect_list_contains_pairs(&guard, &mut pairs);
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&("tags".into(), "vip".into())));
        assert!(pairs.contains(&("roles".into(), "admin".into())));
    }
}
