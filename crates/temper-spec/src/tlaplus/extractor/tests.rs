use super::*;

#[test]
fn test_extract_reference_order_tla() {
    let tla = include_str!("../../../../../test-fixtures/specs/order.tla");
    let sm = extract_state_machine(tla).expect("should extract without error");

    assert_eq!(sm.module_name, "Order");
    assert_reference_states(&sm);
    assert_reference_constants(&sm);
    assert_reference_variables(&sm);
    assert_reference_transitions(&sm);
    assert_reference_invariants(&sm);
    assert_reference_liveness(&sm);
}

#[test]
fn test_extract_module_name() {
    let source = "---- MODULE TestModule ----\n\\* Some comment\n====";
    let name = source::extract_module_name(source).unwrap();
    assert_eq!(name, "TestModule");
}

#[test]
fn test_extract_states_from_set() {
    let source = r#"
States == {"Active", "Inactive", "Deleted"}
"#;
    let states = states::extract_states(source).unwrap();
    assert_eq!(states, vec!["Active", "Inactive", "Deleted"]);
}

#[test]
fn debug_cancel() {
    let tla = include_str!("../../../../../test-fixtures/specs/order.tla");
    let sm = extract_state_machine(tla).unwrap();
    for transition in &sm.transitions {
        if transition.name.contains("Cancel") || transition.name.contains("Initiate") {
            eprintln!(
                "{}: from={:?} to={:?} has_params={}",
                transition.name,
                transition.from_states,
                transition.to_state,
                transition.has_parameters
            );
        }
    }
}

fn assert_reference_states(sm: &StateMachine) {
    assert!(sm.states.contains(&"Draft".to_string()));
    assert!(sm.states.contains(&"Submitted".to_string()));
    assert!(sm.states.contains(&"Shipped".to_string()));
    assert!(sm.states.contains(&"Refunded".to_string()));
    assert_eq!(sm.states.len(), 10);
}

fn assert_reference_constants(sm: &StateMachine) {
    assert!(sm.constants.contains(&"MAX_ITEMS".to_string()));
    assert!(sm.constants.contains(&"MAX_ORDER_TOTAL".to_string()));
}

fn assert_reference_variables(sm: &StateMachine) {
    assert!(sm.variables.contains(&"status".to_string()));
    assert!(sm.variables.contains(&"items".to_string()));
    assert!(sm.variables.contains(&"total".to_string()));
}

fn assert_reference_transitions(sm: &StateMachine) {
    let transition_names: Vec<&str> = sm.transitions.iter().map(|t| t.name.as_str()).collect();
    assert!(
        transition_names.contains(&"SubmitOrder"),
        "should have SubmitOrder, got: {transition_names:?}"
    );
    assert!(transition_names.contains(&"ConfirmOrder"));
    assert!(transition_names.contains(&"ShipOrder"));
    assert!(transition_names.contains(&"DeliverOrder"));
    assert!(
        transition_names.contains(&"CancelOrder"),
        "got: {transition_names:?}"
    );
    assert!(transition_names.contains(&"InitiateReturn"));

    let submit = sm
        .transitions
        .iter()
        .find(|transition| transition.name == "SubmitOrder")
        .unwrap();
    assert_eq!(submit.to_state, Some("Submitted".to_string()));
}

fn assert_reference_invariants(sm: &StateMachine) {
    assert!(!sm.invariants.is_empty(), "should have invariants");
    let names: Vec<&str> = sm
        .invariants
        .iter()
        .map(|invariant| invariant.name.as_str())
        .collect();
    assert!(
        names.contains(&"TypeInvariant"),
        "should have TypeInvariant, got: {names:?}"
    );
    assert!(names.contains(&"ShipRequiresPayment"));
}

fn assert_reference_liveness(sm: &StateMachine) {
    assert!(
        !sm.liveness_properties.is_empty(),
        "should have liveness properties"
    );
    let names: Vec<&str> = sm
        .liveness_properties
        .iter()
        .map(|property| property.name.as_str())
        .collect();
    assert!(
        names.contains(&"SubmittedProgress"),
        "should have SubmittedProgress, got: {names:?}"
    );
}
