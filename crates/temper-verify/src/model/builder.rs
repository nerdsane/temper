//! Model builder: constructs a `TemperModel` directly from I/O Automaton specifications.
//!
//! Translates `Automaton` (parsed IOA TOML) into pre-computed structures for
//! efficient Stateright model checking. No TLA+ or StateMachine IR involved.

use std::collections::BTreeMap;

use temper_spec::automaton::{
    self, Automaton, parse_bool_initial, parse_counter_initial_usize, parse_list_initial,
};

use super::types::{
    InvariantKind, LivenessKind, ModelEffect, ModelGuard, ResolvedInvariant, ResolvedLiveness,
    ResolvedTransition, TemperModel,
};

/// Build a `TemperModel` from I/O Automaton TOML source.
///
/// This is the sole entry point. The IOA format has explicit guards and effects,
/// so the `Automaton` is translated directly — no intermediate representation.
pub fn build_model_from_ioa(ioa_toml: &str, max_counter: usize) -> TemperModel {
    let automaton = temper_spec::automaton::parse_automaton(ioa_toml)
        .expect("failed to parse I/O Automaton TOML");
    build_model_from_automaton(&automaton, max_counter)
}

/// Build a `TemperModel` directly from a parsed [`Automaton`].
pub fn build_model_from_automaton(automaton: &Automaton, max_counter: usize) -> TemperModel {
    let states = automaton.automaton.states.clone();
    let initial_status = automaton.automaton.initial.clone();

    // Extract initial values from [[state]] declarations.
    let mut initial_counters = BTreeMap::new();
    let mut initial_booleans = BTreeMap::new();
    let mut initial_lists = BTreeMap::new();
    let mut counter_bounds = BTreeMap::new();

    for sv in &automaton.state {
        match sv.var_type.as_str() {
            "counter" => {
                let init_val = parse_counter_initial_usize(&sv.initial);
                initial_counters.insert(sv.name.clone(), init_val);
                counter_bounds.insert(sv.name.clone(), max_counter);
            }
            "bool" => {
                let init_val = parse_bool_initial(&sv.initial);
                initial_booleans.insert(sv.name.clone(), init_val);
            }
            "list" | "set" => {
                initial_lists.insert(sv.name.clone(), parse_list_initial(&sv.initial));
            }
            _ => {
                // Keep verification robust against partially modeled types.
                // Semantic linting reports unsupported state variable types.
            }
        }
    }

    let transitions = resolve_transitions(automaton, &initial_counters);
    let invariants = resolve_invariants(automaton);
    let liveness = resolve_liveness(automaton);

    TemperModel {
        states,
        transitions,
        invariants,
        liveness,
        initial_status,
        initial_counters,
        initial_booleans,
        initial_lists,
        counter_bounds,
        default_max_counter: max_counter,
    }
}

/// Translate IOA actions into resolved transitions with model guards and effects.
fn resolve_transitions(
    automaton: &Automaton,
    counter_vars: &BTreeMap<String, usize>,
) -> Vec<ResolvedTransition> {
    automaton
        .actions
        .iter()
        .filter(|a| a.kind != "output") // Output actions don't transition state
        .map(|a| {
            // Build guard from IOA guard clauses
            let guard = translate_guards(&a.guard);

            // Build effects from IOA effect clauses
            let mut effects = translate_effects(&a.effect);

            // Name-heuristic fallback: only when no explicit effects
            if a.effect.is_empty() {
                let name_lower = a.name.to_lowercase();
                if name_lower.contains("additem") || name_lower.contains("add_item") {
                    effects.push(ModelEffect::IncrementCounter("items".to_string()));
                    for var in counter_vars.keys() {
                        if var != "items" {
                            effects.push(ModelEffect::IncrementCounter(var.clone()));
                        }
                    }
                } else if name_lower.contains("removeitem") || name_lower.contains("remove_item") {
                    effects.push(ModelEffect::DecrementCounter("items".to_string()));
                    for var in counter_vars.keys() {
                        if var != "items" {
                            effects.push(ModelEffect::DecrementCounter(var.clone()));
                        }
                    }
                }
            }

            ResolvedTransition {
                name: a.name.clone(),
                from_states: a.from.clone(),
                to_state: a.to.clone(),
                guard,
                effects,
            }
        })
        .collect()
}

/// Translate IOA guard clauses to a ModelGuard.
fn translate_guards(guards: &[automaton::Guard]) -> ModelGuard {
    let model_guards: Vec<ModelGuard> = guards
        .iter()
        .map(|g| match g {
            automaton::Guard::StateIn { values } => ModelGuard::StateIn(values.clone()),
            automaton::Guard::MinCount { var, min } => ModelGuard::CounterMin {
                var: var.clone(),
                min: *min,
            },
            automaton::Guard::MaxCount { var, max } => ModelGuard::CounterMax {
                var: var.clone(),
                max: *max,
            },
            automaton::Guard::IsTrue { var } => ModelGuard::BoolTrue(var.clone()),
            automaton::Guard::ListContains { var, value } => ModelGuard::ListContains {
                var: var.clone(),
                value: value.clone(),
            },
            automaton::Guard::ListLengthMin { var, min } => ModelGuard::ListLengthMin {
                var: var.clone(),
                min: *min,
            },
        })
        .collect();

    match model_guards.len() {
        0 => ModelGuard::Always,
        1 => model_guards.into_iter().next().unwrap(), // ci-ok: len() == 1
        _ => ModelGuard::And(model_guards),
    }
}

/// Translate IOA effect clauses to ModelEffects.
fn translate_effects(effects: &[automaton::Effect]) -> Vec<ModelEffect> {
    effects
        .iter()
        .filter_map(|e| match e {
            automaton::Effect::Increment { var } => {
                Some(ModelEffect::IncrementCounter(var.clone()))
            }
            automaton::Effect::Decrement { var } => {
                Some(ModelEffect::DecrementCounter(var.clone()))
            }
            automaton::Effect::SetBool { var, value } => Some(ModelEffect::SetBool {
                var: var.clone(),
                value: *value,
            }),
            automaton::Effect::ListAppend { var } => Some(ModelEffect::ListAppend(var.clone())),
            automaton::Effect::ListRemoveAt { var } => Some(ModelEffect::ListRemoveAt(var.clone())),
            automaton::Effect::Emit { .. } => None, // Emit is runtime-only
            automaton::Effect::Trigger { .. } => None, // Trigger is runtime-only (WASM dispatch)\
            automaton::Effect::Schedule { .. } => None, // Schedule is runtime-only (timer dispatch)
        })
        .collect()
}

/// Translate IOA invariants into resolved invariants.
///
/// Invariant classification:
/// - `"items > 0"` or `"counter_name > 0"` → `CounterPositive`
/// - bare identifier (e.g. `"payment_captured"`) → `BoolRequired`
/// - `"no_further_transitions"` → `NoFurtherTransitions`
/// - everything else → `Implication` (fallback)
///
/// A `TypeInvariant` (StatusInSet) is always auto-included.
fn resolve_invariants(automaton: &Automaton) -> Vec<ResolvedInvariant> {
    let mut result = Vec::new();

    // Auto-include TypeInvariant
    result.push(ResolvedInvariant {
        name: "TypeInvariant".to_string(),
        trigger_states: vec![],
        required_states: vec![],
        kind: InvariantKind::StatusInSet,
    });

    // Collect known variable names for classification
    let counter_names: Vec<&str> = automaton
        .state
        .iter()
        .filter(|s| s.var_type == "counter")
        .map(|s| s.name.as_str())
        .collect();
    let bool_names: Vec<&str> = automaton
        .state
        .iter()
        .filter(|s| s.var_type == "bool")
        .map(|s| s.name.as_str())
        .collect();

    for inv in &automaton.invariants {
        let expr = inv.assert.trim();

        // "no_further_transitions" → NoFurtherTransitions
        if expr == "no_further_transitions" {
            result.push(ResolvedInvariant {
                name: inv.name.clone(),
                trigger_states: inv.when.clone(),
                required_states: vec![],
                kind: InvariantKind::NoFurtherTransitions,
            });
            continue;
        }

        // "var > 0" → CounterPositive
        if expr.contains("> 0") {
            let var = expr.split('>').next().unwrap_or("").trim();
            if !var.is_empty()
                && (counter_names.contains(&var) || var == "items" || var == "item_count")
            {
                result.push(ResolvedInvariant {
                    name: inv.name.clone(),
                    trigger_states: inv.when.clone(),
                    required_states: vec![],
                    kind: InvariantKind::CounterPositive {
                        var: var.to_string(),
                    },
                });
                continue;
            }
        }

        // Bare identifier → BoolRequired (if it's a known boolean)
        if !expr.contains(' ') && bool_names.contains(&expr) {
            result.push(ResolvedInvariant {
                name: inv.name.clone(),
                trigger_states: inv.when.clone(),
                required_states: vec![],
                kind: InvariantKind::BoolRequired {
                    var: expr.to_string(),
                },
            });
            continue;
        }

        // Fallback: treat as Implication
        result.push(ResolvedInvariant {
            name: inv.name.clone(),
            trigger_states: inv.when.clone(),
            required_states: vec![],
            kind: InvariantKind::Implication,
        });
    }

    result
}

/// Translate IOA liveness properties into resolved liveness.
fn resolve_liveness(automaton: &Automaton) -> Vec<ResolvedLiveness> {
    automaton
        .liveness
        .iter()
        .map(|l| {
            let kind = if !l.reaches.is_empty() {
                LivenessKind::ReachesState {
                    from: l.from.clone(),
                    targets: l.reaches.clone(),
                }
            } else if l.has_actions == Some(true) {
                LivenessKind::NoDeadlock {
                    from: l.from.clone(),
                }
            } else {
                // Default: treat as reachability with empty targets (trivially true)
                LivenessKind::ReachesState {
                    from: l.from.clone(),
                    targets: vec![],
                }
            };
            ResolvedLiveness {
                name: l.name.clone(),
                kind,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use stateright::Model;

    const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

    fn build_order_model() -> TemperModel {
        build_model_from_ioa(ORDER_IOA, 2)
    }

    #[test]
    fn test_build_model_has_correct_states() {
        let model = build_order_model();
        assert_eq!(model.states.len(), 10);
        assert!(model.states.contains(&"Draft".to_string()));
        assert!(model.states.contains(&"Submitted".to_string()));
        assert!(model.states.contains(&"Confirmed".to_string()));
        assert!(model.states.contains(&"Refunded".to_string()));
    }

    #[test]
    fn test_build_model_initial_state_is_draft() {
        let model = build_order_model();
        let init = model.init_states();
        assert_eq!(init.len(), 1);
        assert_eq!(init[0].status, "Draft");
        assert_eq!(*init[0].counters.get("items").unwrap_or(&99), 0);
    }

    #[test]
    fn test_draft_actions_include_add_item() {
        let model = build_order_model();
        let state = super::super::types::TemperModelState {
            status: "Draft".to_string(),
            counters: BTreeMap::from([("items".to_string(), 0)]),
            booleans: BTreeMap::from([("has_address".to_string(), false)]),
            lists: BTreeMap::new(),
        };
        let mut actions = Vec::new();
        model.actions(&state, &mut actions);
        let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
        assert!(
            names.contains(&"AddItem"),
            "Draft state should allow AddItem, got: {names:?}"
        );
    }

    #[test]
    fn test_submitted_does_not_allow_add_item() {
        let model = build_order_model();
        let state = super::super::types::TemperModelState {
            status: "Submitted".to_string(),
            counters: BTreeMap::from([("items".to_string(), 1)]),
            booleans: BTreeMap::from([("has_address".to_string(), true)]),
            lists: BTreeMap::new(),
        };
        let mut actions = Vec::new();
        model.actions(&state, &mut actions);
        let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
        assert!(
            !names.contains(&"AddItem"),
            "Submitted state should NOT allow AddItem, got: {names:?}"
        );
    }

    #[test]
    fn test_draft_to_submitted_transition() {
        let model = build_order_model();
        let state = super::super::types::TemperModelState {
            status: "Draft".to_string(),
            counters: BTreeMap::from([("items".to_string(), 1)]),
            booleans: BTreeMap::from([("has_address".to_string(), false)]),
            lists: BTreeMap::new(),
        };
        let action = super::super::types::TemperModelAction {
            name: "SubmitOrder".to_string(),
            target_state: Some("Submitted".to_string()),
        };
        let next = model.next_state(&state, action);
        assert!(next.is_some());
        let next = next.unwrap();
        assert_eq!(next.status, "Submitted");
        assert_eq!(*next.counters.get("items").unwrap(), 1);
    }

    #[test]
    fn test_add_item_increments_count() {
        let model = build_order_model();
        let state = super::super::types::TemperModelState {
            status: "Draft".to_string(),
            counters: BTreeMap::from([("items".to_string(), 0)]),
            booleans: BTreeMap::from([("has_address".to_string(), false)]),
            lists: BTreeMap::new(),
        };
        let action = super::super::types::TemperModelAction {
            name: "AddItem".to_string(),
            target_state: None,
        };
        let next = model.next_state(&state, action).unwrap();
        assert_eq!(*next.counters.get("items").unwrap(), 1);
        assert_eq!(next.status, "Draft");
    }

    #[test]
    fn test_properties_are_generated() {
        let model = build_order_model();
        let props = model.properties();
        assert!(!props.is_empty(), "Model should have at least one property");
    }

    #[test]
    fn test_counter_positive_invariant_resolved() {
        let model = build_order_model();
        let counter_pos = model
            .invariants
            .iter()
            .find(|i| matches!(i.kind, InvariantKind::CounterPositive { .. }));
        assert!(
            counter_pos.is_some(),
            "Should have a CounterPositive invariant"
        );
    }

    #[test]
    fn test_no_further_transitions_invariant_resolved() {
        let model = build_order_model();
        let nft = model
            .invariants
            .iter()
            .find(|i| matches!(i.kind, InvariantKind::NoFurtherTransitions));
        assert!(
            nft.is_some(),
            "Should have a NoFurtherTransitions invariant"
        );
    }

    #[test]
    fn test_undeclared_bool_invariant_falls_back_to_implication() {
        // payment_captured is NOT declared as a [[state]] bool var in the spec,
        // so "ShipRequiresPayment" falls back to Implication (we can't model it).
        let model = build_order_model();
        let ship_inv = model
            .invariants
            .iter()
            .find(|i| i.name == "ShipRequiresPayment");
        assert!(
            ship_inv.is_some(),
            "Should have ShipRequiresPayment invariant"
        );
        assert!(
            matches!(ship_inv.unwrap().kind, InvariantKind::Implication),
            "Undeclared bool should fall back to Implication"
        );
    }

    #[test]
    fn debug_resolved_transitions() {
        let model = build_model_from_ioa(ORDER_IOA, 2);
        for t in &model.transitions {
            eprintln!(
                "{}: from={:?} to={:?} guard={:?} effects={:?}",
                t.name, t.from_states, t.to_state, t.guard, t.effects
            );
        }
    }
}
