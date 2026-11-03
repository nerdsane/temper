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
        strings: BTreeMap::new(),
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
        strings: BTreeMap::new(),
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
        strings: BTreeMap::new(),
    };
    let action = super::super::types::TemperModelAction {
        name: "SubmitOrder".to_string(),
        target_state: Some("Submitted".to_string()),
        params: BTreeMap::new(),
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
        strings: BTreeMap::new(),
    };
    let action = super::super::types::TemperModelAction {
        name: "AddItem".to_string(),
        target_state: None,
        params: BTreeMap::new(),
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
fn test_counter_invariant_resolved() {
    let model = build_order_model();
    assert!(
        model
            .invariants
            .iter()
            .any(|i| i.assert.to_string().ends_with("=> items > 0")),
        "Should have an `items > 0` invariant"
    );
}

#[test]
fn test_terminal_states_resolved() {
    let model = build_order_model();
    assert!(!model.terminal.is_empty(), "Should have terminal states");
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
guard = "Child[child_id].status in ['Done']"
"#;
    let model = build_model_from_ioa(spec, 2).unwrap();
    let guard = &model
        .transitions
        .iter()
        .find(|transition| transition.name == "Proceed")
        .expect("Proceed transition")
        .guard;
    assert_eq!(guard.to_string(), "Child[child_id].status in ['Done']");
    let state = model.init_states().remove(0);
    assert!(
        super::super::semantics::guard_may_hold(guard, &model.var_kinds, &state),
        "a related-entity guard may hold (free boolean)"
    );
    assert!(
        !super::super::semantics::evaluate_guard(guard, &model.var_kinds, &state),
        "a related-entity guard is not locally enabled"
    );
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
initial = false

[[state]]
name = "typecheck_ok"
type = "bool"
initial = false

[[state]]
name = "unit_tests_ok"
type = "bool"
initial = false

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
assert = "status in ['Testing', 'Shipped'] => migrations_ok && typecheck_ok && unit_tests_ok"

[[invariant]]
name = "EitherReviewer"
assert = "status in ['Shipped'] => migrations_ok || typecheck_ok"
"#;

#[test]
fn test_compound_invariants_keep_their_structure() {
    let model = build_model_from_ioa(COMPOUND_IOA, 2).unwrap();
    let text = |name: &str| {
        model
            .invariants
            .iter()
            .find(|i| i.name == name)
            .map(|i| i.assert.to_string())
            .unwrap()
    };
    assert_eq!(
        text("TestingRequiresAllGates"),
        "status in ['Testing', 'Shipped'] => migrations_ok && typecheck_ok && unit_tests_ok"
    );
    assert_eq!(
        text("EitherReviewer"),
        "status in ['Shipped'] => migrations_ok || typecheck_ok"
    );
}

const COMPOUND_UNDECLARED_IOA: &str = r#"
[automaton]
name = "Release"
states = ["Planning", "Shipped"]
initial = "Planning"

[[state]]
name = "migrations_ok"
type = "bool"
initial = false

[[action]]
name = "Ship"
kind = "internal"
from = ["Planning"]
to = "Shipped"

[[invariant]]
name = "MixedDeclaredUndeclared"
assert = "status in ['Shipped'] => migrations_ok && undeclared_flag"
"#;

#[test]
fn test_invariant_over_undeclared_variable_fails_to_load() {
    let err = build_model_from_ioa(COMPOUND_UNDECLARED_IOA, 2)
        .expect_err("undeclared variable must be rejected");
    assert!(
        err.contains("unknown state variable 'undeclared_flag'"),
        "{err}"
    );
}
