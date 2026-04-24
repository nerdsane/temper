//! Model builder: constructs a `TemperModel` directly from I/O Automaton specifications.
//!
//! Uses the shared translation layer in `temper-spec` for guard/effect translation,
//! then converts to verification-specific types. Runtime-only effects (Emit, Trigger,
//! Schedule, Spawn) are filtered out; CrossEntityState guards become Always (permissive).

use std::collections::BTreeMap;

use temper_spec::automaton::{
    Automaton, ParsedAssert, ResolvedEffect, ResolvedGuard, parse_assert_expr, parse_bool_initial,
    parse_counter_initial_usize, parse_list_initial, translate_actions,
};

use super::types::{
    InvariantKind, LivenessKind, ModelEffect, ModelGuard, ResolvedInvariant, ResolvedLiveness,
    ResolvedTransition, TemperModel,
};

/// Build a `TemperModel` from I/O Automaton TOML source.
///
/// This is the sole entry point. The IOA format has explicit guards and effects,
/// so the `Automaton` is translated directly — no intermediate representation.
///
/// Returns an error if the IOA TOML fails to parse.
pub fn build_model_from_ioa(ioa_toml: &str, max_counter: usize) -> Result<TemperModel, String> {
    let automaton = temper_spec::automaton::parse_automaton(ioa_toml)
        .map_err(|e| format!("failed to parse I/O Automaton TOML: {e}"))?;
    Ok(build_model_from_automaton(&automaton, max_counter))
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

    let transitions = resolve_transitions(automaton);
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

/// Translate IOA actions into resolved transitions using the shared translation layer.
fn resolve_transitions(automaton: &Automaton) -> Vec<ResolvedTransition> {
    translate_actions(automaton)
        .into_iter()
        .map(|a| ResolvedTransition {
            name: a.name,
            from_states: a.from_states,
            to_state: a.to_state,
            guard: convert_guard(a.guard),
            effects: a
                .effects
                .into_iter()
                .filter(|e| e.is_verifiable())
                .map(convert_effect)
                .collect(),
        })
        .collect()
}

/// Convert a shared [`ResolvedGuard`] to the verification [`ModelGuard`].
///
/// `CrossEntityState` guards become `Always` (permissive) because cross-entity
/// state is a runtime concern pre-resolved at dispatch time.
fn convert_guard(guard: ResolvedGuard) -> ModelGuard {
    match guard {
        ResolvedGuard::Always => ModelGuard::Always,
        ResolvedGuard::StateIn(values) => ModelGuard::StateIn(values),
        ResolvedGuard::CounterMin { var, min } => ModelGuard::CounterMin { var, min },
        ResolvedGuard::CounterMax { var, max } => ModelGuard::CounterMax { var, max },
        ResolvedGuard::BoolTrue(var) => ModelGuard::BoolTrue(var),
        ResolvedGuard::BoolFalse(var) => ModelGuard::BoolFalse(var),
        ResolvedGuard::ListContains { var, value } => ModelGuard::ListContains { var, value },
        ResolvedGuard::ListLengthMin { var, min } => ModelGuard::ListLengthMin { var, min },
        ResolvedGuard::CrossEntityState { .. } => {
            // Cross-entity guards are runtime-only (pre-resolved at dispatch).
            // For model checking, treat as always-true (permissive).
            ModelGuard::Always
        }
        ResolvedGuard::And(guards) => {
            ModelGuard::And(guards.into_iter().map(convert_guard).collect())
        }
    }
}

/// Convert a verifiable [`ResolvedEffect`] to the verification [`ModelEffect`].
///
/// Only called for effects where `is_verifiable()` is true. Runtime-only
/// effects are filtered before reaching this function.
fn convert_effect(effect: ResolvedEffect) -> ModelEffect {
    match effect {
        ResolvedEffect::IncrementCounter(var) => ModelEffect::IncrementCounter(var),
        ResolvedEffect::DecrementCounter(var) => ModelEffect::DecrementCounter(var),
        ResolvedEffect::SetBool { var, value } => ModelEffect::SetBool { var, value },
        ResolvedEffect::ListAppend(var) => ModelEffect::ListAppend(var),
        ResolvedEffect::ListRemoveAt(var) => ModelEffect::ListRemoveAt(var),
        // Runtime-only effects should have been filtered by is_verifiable()
        ResolvedEffect::Emit(_)
        | ResolvedEffect::Trigger(_)
        | ResolvedEffect::IncrementCounterByParam { .. }
        | ResolvedEffect::DecrementCounterByParam { .. }
        | ResolvedEffect::Schedule { .. }
        | ResolvedEffect::ScheduleAt { .. }
        | ResolvedEffect::Spawn { .. } => {
            unreachable!("runtime-only effect should have been filtered")
        }
    }
}

/// Translate IOA invariants into resolved invariants.
///
/// Uses [`parse_assert_expr`] from `temper-spec` as the primary classifier,
/// then falls back to known boolean variable names. Unrecognized expressions
/// become `Unverifiable` (with a warning) instead of silently passing.
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

    // Collect known boolean variable names for fallback classification
    let bool_names: Vec<&str> = automaton
        .state
        .iter()
        .filter(|s| s.var_type == "bool")
        .map(|s| s.name.as_str())
        .collect();

    for inv in &automaton.invariants {
        let expr = inv.assert.trim();

        let kind = match parse_assert_expr(expr) {
            Some(parsed) => translate_parsed_assert(parsed, expr, &bool_names),
            None => InvariantKind::Unverifiable {
                expression: expr.to_string(),
            },
        };
        result.push(ResolvedInvariant {
            name: inv.name.clone(),
            trigger_states: inv.when.clone(),
            required_states: vec![],
            kind,
        });
    }

    result
}

/// Translate a [`ParsedAssert`] into an [`InvariantKind`].
///
/// Bare boolean references are only accepted when the variable is declared
/// as a `bool` in the automaton state; otherwise the whole expression falls
/// through to `Unverifiable`. This preserves the pre-compound behavior of
/// rejecting unknown identifiers rather than silently asserting on them.
fn translate_parsed_assert(parsed: ParsedAssert, raw: &str, bool_names: &[&str]) -> InvariantKind {
    match try_translate(&parsed, bool_names) {
        Some(kind) => kind,
        None => InvariantKind::Unverifiable {
            expression: raw.to_string(),
        },
    }
}

fn try_translate(parsed: &ParsedAssert, bool_names: &[&str]) -> Option<InvariantKind> {
    match parsed {
        ParsedAssert::CounterPositive { var } => {
            Some(InvariantKind::CounterPositive { var: var.clone() })
        }
        ParsedAssert::NoFurtherTransitions => Some(InvariantKind::NoFurtherTransitions),
        ParsedAssert::NeverState { state } => Some(InvariantKind::NeverState {
            state: state.clone(),
        }),
        ParsedAssert::CounterCompare { var, op, value } => Some(InvariantKind::CounterCompare {
            var: var.clone(),
            op: op.clone(),
            value: *value,
        }),
        ParsedAssert::BoolRequired { var, expect } => {
            if bool_names.contains(&var.as_str()) {
                Some(InvariantKind::BoolRequired {
                    var: var.clone(),
                    expect: *expect,
                })
            } else {
                None
            }
        }
        ParsedAssert::OrderingConstraint { .. } => None,
        ParsedAssert::And(parts) => {
            let mapped: Option<Vec<_>> =
                parts.iter().map(|p| try_translate(p, bool_names)).collect();
            mapped.map(InvariantKind::And)
        }
        ParsedAssert::Or(parts) => {
            let mapped: Option<Vec<_>> =
                parts.iter().map(|p| try_translate(p, bool_names)).collect();
            mapped.map(InvariantKind::Or)
        }
    }
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
        build_model_from_ioa(ORDER_IOA, 2).unwrap()
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
    fn test_undeclared_bool_invariant_falls_back_to_unverifiable() {
        // payment_captured is NOT declared as a [[state]] bool var in the spec,
        // so "ShipRequiresPayment" falls back to Unverifiable (we can't model it).
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
            matches!(ship_inv.unwrap().kind, InvariantKind::Unverifiable { .. }),
            "Undeclared bool should fall back to Unverifiable"
        );
    }

    #[test]
    fn debug_resolved_transitions() {
        let model = build_model_from_ioa(ORDER_IOA, 2).unwrap();
        for t in &model.transitions {
            eprintln!(
                "{}: from={:?} to={:?} guard={:?} effects={:?}",
                t.name, t.from_states, t.to_state, t.guard, t.effects
            );
        }
    }

    // --- Compound invariant tests ---------------------------------------

    const COMPOUND_IOA: &str = r#"
[automaton]
name = "Release"
states = ["Planning", "Testing", "Shipped"]
initial = "Planning"

[[state]]
name = "migrations_ok"
type = "bool"
initial = "false"

[[state]]
name = "typecheck_ok"
type = "bool"
initial = "false"

[[state]]
name = "unit_tests_ok"
type = "bool"
initial = "false"

[[action]]
name = "EnterTesting"
kind = "input"
from = ["Planning"]
to = "Testing"

[[action]]
name = "Ship"
kind = "internal"
from = ["Testing"]
to = "Shipped"

[[invariant]]
name = "TestingRequiresAllGates"
when = ["Testing", "Shipped"]
assert = "migrations_ok && typecheck_ok && unit_tests_ok"

[[invariant]]
name = "EitherReviewer"
when = ["Shipped"]
assert = "migrations_ok || typecheck_ok"
"#;

    #[test]
    fn test_compound_and_invariant_resolves_to_and() {
        let model = build_model_from_ioa(COMPOUND_IOA, 2).unwrap();
        let inv = model
            .invariants
            .iter()
            .find(|i| i.name == "TestingRequiresAllGates")
            .expect("TestingRequiresAllGates invariant must be present");
        match &inv.kind {
            InvariantKind::And(parts) => {
                assert_eq!(parts.len(), 3);
                for p in parts {
                    assert!(
                        matches!(p, InvariantKind::BoolRequired { expect: true, .. }),
                        "part should be BoolRequired{{expect:true}}, got {p:?}"
                    );
                }
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn test_compound_or_invariant_resolves_to_or() {
        let model = build_model_from_ioa(COMPOUND_IOA, 2).unwrap();
        let inv = model
            .invariants
            .iter()
            .find(|i| i.name == "EitherReviewer")
            .expect("EitherReviewer invariant must be present");
        match &inv.kind {
            InvariantKind::Or(parts) => {
                assert_eq!(parts.len(), 2);
            }
            other => panic!("expected Or, got {other:?}"),
        }
    }

    const COMPOUND_UNDECLARED_IOA: &str = r#"
[automaton]
name = "Release"
states = ["Planning", "Shipped"]
initial = "Planning"

[[state]]
name = "migrations_ok"
type = "bool"
initial = "false"

[[action]]
name = "Ship"
kind = "internal"
from = ["Planning"]
to = "Shipped"

[[invariant]]
name = "MixedDeclaredUndeclared"
when = ["Shipped"]
assert = "migrations_ok && undeclared_flag"
"#;

    #[test]
    fn test_compound_with_undeclared_bool_becomes_unverifiable() {
        let model = build_model_from_ioa(COMPOUND_UNDECLARED_IOA, 2).unwrap();
        let inv = model
            .invariants
            .iter()
            .find(|i| i.name == "MixedDeclaredUndeclared")
            .unwrap();
        assert!(
            matches!(inv.kind, InvariantKind::Unverifiable { .. }),
            "compound expression referencing an undeclared bool must fall back to Unverifiable"
        );
    }
}
