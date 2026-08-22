//! Model builder: constructs a `TemperModel` directly from I/O Automaton specifications.
//!
//! Uses the shared translation layer in `temper-spec` for guard/effect translation,
//! then converts to verification-specific types. Runtime-only effects (Emit, Trigger,
//! Schedule, Spawn) are filtered out; CrossEntityState guards are kept as abstract
//! guards so single-entity checks do not silently treat them as locally enabled.

use std::collections::BTreeMap;

use temper_spec::automaton::{
    Automaton, parse_bool_initial, parse_counter_initial_usize, parse_list_initial,
};

use super::resolution::{resolve_invariants, resolve_liveness, resolve_transitions};
use super::types::TemperModel;
#[cfg(test)]
use super::types::{InvariantKind, ModelGuard};

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
    let initial_counter_variants =
        super::reference_contract::initialize_reference_counters(automaton, &mut initial_counters);
    let mut reference_properties_by_type = BTreeMap::<String, Vec<String>>::new();
    for state in automaton
        .state
        .iter()
        .filter(|state| state.var_type == "ref")
    {
        reference_properties_by_type
            .entry(state.entity_type.clone().unwrap_or_default())
            .or_default()
            .push(state.name.clone());
    }
    let identity_properties = automaton
        .keys
        .iter()
        .find(|key| key.entity_id)
        .map(|key| key.properties.clone())
        .unwrap_or_default();

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
        initial_counter_variants,
        reference_properties_by_type,
        identity_properties,
        initial_booleans,
        initial_lists,
        counter_bounds,
        default_max_counter: max_counter,
    }
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
            reference_params: BTreeMap::new(),
            fresh_references: BTreeMap::new(),
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
            reference_params: BTreeMap::new(),
            fresh_references: BTreeMap::new(),
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
    fn test_cross_entity_guard_is_preserved_as_abstract_guard() {
        let spec = r#"
[automaton]
name = "Parent"
states = ["Waiting", "Ready"]
initial = "Waiting"

[[action]]
name = "Proceed"
from = ["Waiting"]
to = "Ready"
guard = [{ type = "cross_entity_state", entity_type = "Child", entity_id_source = "child_id", required_status = ["Done"] }]
"#;
        let model = build_model_from_ioa(spec, 2).unwrap();
        let guard = &model
            .transitions
            .iter()
            .find(|transition| transition.name == "Proceed")
            .expect("Proceed transition")
            .guard;

        assert!(
            guard.contains_cross_entity(),
            "cross-entity guard must not collapse to Always"
        );
        match guard {
            ModelGuard::And(parts) => assert!(parts.iter().any(|part| matches!(
                part,
                ModelGuard::CrossEntityState {
                    entity_type,
                    entity_id_source,
                    required_status,
                    ..
                } if entity_type == "Child"
                    && entity_id_source == "child_id"
                    && required_status == &vec!["Done".to_string()]
            ))),
            other => panic!("expected compound guard with CrossEntityState, got {other:?}"),
        }
    }

    #[test]
    fn test_undeclared_bool_invariant_falls_back_to_unverifiable() {
        const UNVERIFIABLE_IOA: &str = r#"
[automaton]
name = "UnverifiableEvidence"
states = ["Open"]
initial = "Open"

[[invariant]]
name = "RequiresUndeclaredEvidence"
assert = "undeclared_flag"
"#;
        let model = build_model_from_ioa(UNVERIFIABLE_IOA, 2).expect("test spec parses");
        let invariant = model
            .invariants
            .iter()
            .find(|i| i.name == "RequiresUndeclaredEvidence");
        assert!(
            invariant.is_some(),
            "Should retain the unverifiable invariant"
        );
        assert!(
            matches!(invariant.unwrap().kind, InvariantKind::Unverifiable { .. }),
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
