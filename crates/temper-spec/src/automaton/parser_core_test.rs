use super::super::*;
use super::ORDER_IOA;

#[test]
fn test_parse_order_automaton() {
    let automaton = parse_automaton(ORDER_IOA).expect("should parse");
    assert_eq!(automaton.automaton.name, "Order");
    assert_eq!(automaton.automaton.initial, "Draft");
    assert_eq!(automaton.automaton.states.len(), 10);
    assert!(automaton.automaton.states.contains(&"Draft".to_string()));
    assert!(automaton.automaton.states.contains(&"Shipped".to_string()));
}

#[test]
fn test_automaton_version_round_trips() {
    let spec = r#"
[automaton]
name = "Versioned"
version = "1.0.0"
states = ["Active"]
initial = "Active"
"#;

    let parsed = parse_automaton(spec).expect("versioned automaton should parse");
    let canonical = toml::to_string(&parsed).expect("versioned automaton should serialize");
    assert!(canonical.contains("version = \"1.0.0\""));
    parse_automaton(&canonical).expect("canonical versioned automaton should reparse");
}

#[test]
fn test_actions_parsed() {
    let automaton = parse_automaton(ORDER_IOA).unwrap();
    let names: Vec<&str> = automaton
        .actions
        .iter()
        .map(|action| action.name.as_str())
        .collect();
    assert!(names.contains(&"AddItem"), "got: {names:?}");
    assert!(names.contains(&"SubmitOrder"));
    assert!(names.contains(&"CancelOrder"));
    assert!(names.contains(&"ConfirmOrder"));
}

#[test]
fn test_submit_order_has_guard() {
    let automaton = parse_automaton(ORDER_IOA).unwrap();
    let submit = automaton
        .actions
        .iter()
        .find(|action| action.name == "SubmitOrder")
        .unwrap();
    assert_eq!(submit.from, vec!["Draft"]);
    assert_eq!(submit.to, Some("Submitted".to_string()));
    assert!(!submit.guard.is_empty(), "SubmitOrder should have a guard");
}

#[test]
fn test_cancel_from_multiple_states() {
    let automaton = parse_automaton(ORDER_IOA).unwrap();
    let cancel = automaton
        .actions
        .iter()
        .find(|action| action.name == "CancelOrder")
        .unwrap();
    assert_eq!(cancel.from.len(), 3);
    assert!(cancel.from.contains(&"Draft".to_string()));
    assert!(cancel.from.contains(&"Submitted".to_string()));
    assert!(cancel.from.contains(&"Confirmed".to_string()));
}

#[test]
fn test_invariants_parsed() {
    let automaton = parse_automaton(ORDER_IOA).unwrap();
    assert!(!automaton.invariants.is_empty());
    let names: Vec<&str> = automaton
        .invariants
        .iter()
        .map(|invariant| invariant.name.as_str())
        .collect();
    assert!(names.contains(&"SubmitRequiresItems"), "got: {names:?}");
}

#[test]
fn test_state_query_index_flag_parsed() {
    let toml = r#"
[automaton]
name = "ProjectionAware"
states = ["Open"]
initial = "Open"

[[state]]
name = "title"
type = "string"
initial = ""

[[state]]
name = "last_progress_at"
type = "string"
initial = ""
query_indexed = false
"#;

    let automaton = parse_automaton(toml).expect("should parse");
    let title = automaton
        .state
        .iter()
        .find(|state| state.name == "title")
        .expect("title state");
    let progress = automaton
        .state
        .iter()
        .find(|state| state.name == "last_progress_at")
        .expect("last_progress_at state");

    assert_eq!(title.query_indexed, None);
    assert_eq!(progress.query_indexed, Some(false));
}

#[test]
fn test_convert_to_state_machine() {
    let automaton = parse_automaton(ORDER_IOA).unwrap();
    let state_machine = to_state_machine(&automaton);
    assert_eq!(state_machine.module_name, "Order");
    assert_eq!(state_machine.states.len(), 10);
    assert!(!state_machine.transitions.is_empty());
    assert!(!state_machine.invariants.is_empty());

    let submit = state_machine
        .transitions
        .iter()
        .find(|transition| transition.name == "SubmitOrder")
        .unwrap();
    assert_eq!(submit.from_states, vec!["Draft"]);
    assert_eq!(submit.to_state, Some("Submitted".to_string()));
}

#[test]
fn test_invalid_initial_state_rejected() {
    let toml = r#"
[automaton]
name = "Bad"
states = ["A", "B"]
initial = "C"
"#;
    let result = parse_automaton(toml);
    assert!(result.is_err());
}

#[test]
fn test_invalid_from_state_rejected() {
    let toml = r#"
[automaton]
name = "Bad"
states = ["A", "B"]
initial = "A"

[[action]]
name = "Go"
from = ["Z"]
to = "B"
"#;
    let result = parse_automaton(toml);
    assert!(result.is_err());
}
