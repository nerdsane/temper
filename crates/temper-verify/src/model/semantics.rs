//! Shared concrete guard/effect semantics for verification backends.

use std::collections::BTreeSet;

use temper_jit::table::Effect;

use super::types::{ModelGuard, TemperModelState};

/// Evaluate a model guard against a concrete model state for **local
/// enablement**.
///
/// This answers the question "is this guard *guaranteed* satisfied by the local
/// entity's state alone?". It is the predicate used by safety checks that reason
/// about whether an outgoing transition is locally enabled —
/// `no_further_transitions` (locally terminal?) and `no_deadlock` (always has a
/// local action?).
///
/// A [`ModelGuard::CrossEntityState`] depends on a *different* entity that the
/// single-entity model does not track, so it is **not** locally enabled: this
/// arm returns `false`. This keeps "is the entity locally waiting / terminal?"
/// proofs sound — a state whose only exit is a cross-entity gate is correctly
/// treated as a place the local automaton may sit and wait.
///
/// For state-space *exploration* (which edges could ever fire), use
/// [`guard_may_hold`] instead, which treats a cross-entity guard as a free
/// (nondeterministic) boolean.
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
        ModelGuard::CrossEntityState { .. } => false,
        ModelGuard::And(guards) => guards.iter().all(|g| evaluate_guard(g, state)),
    }
}

/// Evaluate a model guard for **state-space exploration** ("could this edge
/// ever fire?").
///
/// Identical to [`evaluate_guard`] for every locally resolvable guard, but
/// treats [`ModelGuard::CrossEntityState`] as a *free (nondeterministic)
/// boolean*: its value is controlled by a related entity the single-entity
/// model does not track, so the environment *may* satisfy it. Returning `true`
/// here yields the **guard-true branch** of that free boolean — the gated edge
/// is offered, so its target state and the properties on it get explored.
///
/// The complementary **guard-false branch** needs no special handling: the BFS
/// also visits every state in which the gated transition is simply not taken,
/// which is exactly "the environment did not satisfy the guard." Together the
/// two branches give the sound free-boolean abstraction.
///
/// This is deliberately *not* used by local-enablement safety checks (see
/// [`evaluate_guard`]): treating the gate as always-true there would mask real
/// deadlock / terminal-state violations.
pub fn guard_may_hold(guard: &ModelGuard, state: &TemperModelState) -> bool {
    match guard {
        // Free boolean: the environment may satisfy a cross-entity gate.
        ModelGuard::CrossEntityState { .. } => true,
        // Compound guards: a conjunction may hold iff every conjunct may hold.
        // Locally resolvable conjuncts are still resolved concretely, so a
        // cross-entity gate combined with an unsatisfiable local condition stays
        // unsatisfiable — the free boolean does not whitewash the whole guard.
        ModelGuard::And(guards) => guards.iter().all(|g| guard_may_hold(g, state)),
        // Everything else is locally resolvable and identical to evaluate_guard.
        ModelGuard::Always
        | ModelGuard::StateIn(_)
        | ModelGuard::CounterMin { .. }
        | ModelGuard::CounterMax { .. }
        | ModelGuard::BoolTrue(_)
        | ModelGuard::BoolFalse(_)
        | ModelGuard::ListContains { .. }
        | ModelGuard::ListLengthMin { .. } => evaluate_guard(guard, state),
    }
}

/// Apply model effects to the provided state via the shared jit apply.
///
/// `action_name` is used to generate deterministic symbolic list elements
/// (`AddItem#1`). That is the verification abstraction; mutation is
/// [`temper_jit::apply::apply_effects`].
pub fn apply_effects(effects: &[Effect], state: &mut TemperModelState, action_name: &str) {
    for effect in effects {
        let params = symbolic_params(state, effect, action_name);
        temper_jit::apply::apply_effects(state, std::slice::from_ref(effect), &params);
    }
}

/// Params that make jit list effects match the model-checking abstraction.
fn symbolic_params(
    state: &TemperModelState,
    effect: &Effect,
    action_name: &str,
) -> serde_json::Value {
    match effect {
        Effect::ListAppend(var) => {
            let next_idx = state.lists.get(var).map_or(0, Vec::len) + 1;
            serde_json::json!({ var: format!("{action_name}#{next_idx}") })
        }
        Effect::ListRemoveAt(var) => {
            serde_json::json!({ format!("{var}_index"): 0 })
        }
        _ => serde_json::json!({}),
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
        ModelGuard::CrossEntityState { .. } => {}
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
    fn cross_entity_guard_is_unresolved_in_single_entity_state() {
        let g = ModelGuard::CrossEntityState {
            entity_type: "Parent".into(),
            entity_id_source: "parent_id".into(),
            required_status: vec!["Ready".into()],
            forbidden_status: vec![],
        };

        // Local enablement: a cross-entity gate is never *guaranteed* enabled
        // from local state, so evaluate_guard stays false.
        assert!(!evaluate_guard(&g, &state("Waiting")));
        assert!(g.contains_cross_entity());
        assert!(
            ModelGuard::And(vec![ModelGuard::Always, g.clone()]).contains_cross_entity(),
            "compound guards retain abstract cross-entity dependency"
        );
    }

    #[test]
    fn cross_entity_guard_may_hold_is_free_boolean() {
        let g = ModelGuard::CrossEntityState {
            entity_type: "Parent".into(),
            entity_id_source: "parent_id".into(),
            required_status: vec!["Ready".into()],
            forbidden_status: vec![],
        };

        // Exploration: the environment *may* satisfy the gate, so the
        // guard-true branch is offered.
        assert!(
            guard_may_hold(&g, &state("Waiting")),
            "cross-entity guard must be treated as a free boolean (possibly true) for exploration"
        );
        // But it is NOT vacuously true everywhere: it stays unresolved for
        // local enablement.
        assert!(!evaluate_guard(&g, &state("Waiting")));
    }

    #[test]
    fn guard_may_hold_matches_evaluate_for_local_guards() {
        // For every locally resolvable guard, guard_may_hold == evaluate_guard.
        let mut s = state("Draft");
        s.counters.insert("items".into(), 1);
        s.booleans.insert("ready".into(), true);
        s.lists.insert("tags".into(), vec!["vip".into()]);

        let guards = [
            ModelGuard::Always,
            ModelGuard::StateIn(vec!["Draft".into()]),
            ModelGuard::StateIn(vec!["Active".into()]),
            ModelGuard::CounterMin {
                var: "items".into(),
                min: 1,
            },
            ModelGuard::CounterMin {
                var: "items".into(),
                min: 5,
            },
            ModelGuard::CounterMax {
                var: "items".into(),
                max: 2,
            },
            ModelGuard::BoolTrue("ready".into()),
            ModelGuard::BoolFalse("ready".into()),
            ModelGuard::ListContains {
                var: "tags".into(),
                value: "vip".into(),
            },
            ModelGuard::ListLengthMin {
                var: "tags".into(),
                min: 1,
            },
        ];
        for g in &guards {
            assert_eq!(
                guard_may_hold(g, &s),
                evaluate_guard(g, &s),
                "guard_may_hold must agree with evaluate_guard on local guard {g:?}"
            );
        }
    }

    #[test]
    fn guard_may_hold_compound_does_not_whitewash_unsatisfiable_local_conjunct() {
        // A cross-entity gate ANDed with a locally-false condition stays false:
        // the free boolean only relaxes the cross-entity conjunct.
        let g = ModelGuard::And(vec![
            ModelGuard::StateIn(vec!["Active".into()]), // false in "Waiting"
            ModelGuard::CrossEntityState {
                entity_type: "Child".into(),
                entity_id_source: "child_id".into(),
                required_status: vec!["Done".into()],
                forbidden_status: vec![],
            },
        ]);
        assert!(
            !guard_may_hold(&g, &state("Waiting")),
            "an unsatisfiable local conjunct must keep the compound guard unfireable"
        );

        // But when the local conjunct holds, the gate is explorable.
        let g_ok = ModelGuard::And(vec![
            ModelGuard::StateIn(vec!["Waiting".into()]),
            ModelGuard::CrossEntityState {
                entity_type: "Child".into(),
                entity_id_source: "child_id".into(),
                required_status: vec!["Done".into()],
                forbidden_status: vec![],
            },
        ]);
        assert!(guard_may_hold(&g_ok, &state("Waiting")));
        assert!(!evaluate_guard(&g_ok, &state("Waiting")));
    }

    #[test]
    fn effect_increment_counter() {
        let mut s = state("A");
        apply_effects(&[Effect::IncrementCounter("x".into())], &mut s, "Act");
        assert_eq!(s.counters["x"], 1);
        apply_effects(&[Effect::IncrementCounter("x".into())], &mut s, "Act");
        assert_eq!(s.counters["x"], 2);
    }

    #[test]
    fn effect_decrement_counter_saturates_at_zero() {
        let mut s = state("A");
        apply_effects(&[Effect::DecrementCounter("x".into())], &mut s, "Act");
        assert_eq!(s.counters["x"], 0); // saturating sub from 0
        s.counters.insert("x".into(), 3);
        apply_effects(&[Effect::DecrementCounter("x".into())], &mut s, "Act");
        assert_eq!(s.counters["x"], 2);
    }

    #[test]
    fn effect_set_bool() {
        let mut s = state("A");
        apply_effects(
            &[Effect::SetBool {
                var: "done".into(),
                value: true,
            }],
            &mut s,
            "Act",
        );
        assert!(s.booleans["done"]);
        apply_effects(
            &[Effect::SetBool {
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
        apply_effects(&[Effect::ListAppend("log".into())], &mut s, "AddItem");
        assert_eq!(s.lists["log"], vec!["AddItem#1"]);
        apply_effects(&[Effect::ListAppend("log".into())], &mut s, "AddItem");
        assert_eq!(s.lists["log"], vec!["AddItem#1", "AddItem#2"]);
    }

    #[test]
    fn effect_list_remove_at() {
        let mut s = state("A");
        s.lists
            .insert("log".into(), vec!["a".into(), "b".into(), "c".into()]);
        apply_effects(&[Effect::ListRemoveAt("log".into())], &mut s, "Act");
        assert_eq!(s.lists["log"], vec!["b", "c"]);
    }

    #[test]
    fn effect_list_remove_at_empty_is_noop() {
        let mut s = state("A");
        s.lists.insert("log".into(), vec![]);
        apply_effects(&[Effect::ListRemoveAt("log".into())], &mut s, "Act");
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
