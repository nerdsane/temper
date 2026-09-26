use super::super::*;

#[test]
fn test_valid_state_var_types_accepted() {
    let spec = r#"
[automaton]
name = "Task"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "is_done"
type = "bool"
initial = "false"

[[state]]
name = "attempt_count"
type = "counter"
initial = "0"

[[action]]
name = "Complete"
kind = "input"
from = ["Open"]
to = "Done"
effect = ["is_done = true"]
"#;
    let result = parse_automaton(spec);
    assert!(
        result.is_ok(),
        "bool and counter types should be accepted: {:?}",
        result.err()
    );
}

#[test]
fn test_extended_guard_syntax_parsed() {
    let spec = r#"
[automaton]
name = "Ticket"
states = ["Open", "Queued", "Closed"]
initial = "Open"

[[state]]
name = "retries"
type = "counter"
initial = "0"

[[state]]
name = "labels"
type = "list"
initial = "[]"

[[action]]
name = "Queue"
from = ["Open"]
to = "Queued"
guard = "retries < 3"

[[action]]
name = "Escalate"
from = ["Queued"]
to = "Queued"
guard = "'urgent' in labels"

[[action]]
name = "Close"
from = ["Queued"]
to = "Closed"
guard = "len(labels) >= 1"
"#;

    let automaton = parse_automaton(spec).expect("extended guard forms should parse");
    let queue = automaton
        .actions
        .iter()
        .find(|action| action.name == "Queue")
        .unwrap();
    assert_eq!(queue.guard.to_string(), "retries < 3");

    let escalate = automaton
        .actions
        .iter()
        .find(|action| action.name == "Escalate")
        .unwrap();
    assert_eq!(escalate.guard.to_string(), "'urgent' in labels");

    let close = automaton
        .actions
        .iter()
        .find(|action| action.name == "Close")
        .unwrap();
    assert_eq!(close.guard.to_string(), "len(labels) >= 1");
}

#[test]
fn test_invalid_guard_number_rejected() {
    let spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "items"
type = "counter"
initial = "0"

[[action]]
name = "SubmitOrder"
from = ["Draft"]
to = "Submitted"
guard = "items > nope"
"#;

    let err = parse_automaton(spec).expect_err("invalid numeric guard should fail");
    assert!(
        err.to_string().contains("unknown state variable 'nope'"),
        "{err}"
    );
}

#[test]
fn test_parse_schedule_effect() {
    let spec = r#"
[automaton]
name = "OAuthToken"
states = ["Active", "Refreshing", "Expired"]
initial = "Active"

[[action]]
name = "Activate"
from = ["Refreshing"]
to = "Active"
effect = ["schedule('Refresh', 2700)"]

[[action]]
name = "Refresh"
from = ["Active"]
to = "Refreshing"
"#;

    let automaton = parse_automaton(spec).expect("should parse schedule effect");
    let activate = automaton
        .actions
        .iter()
        .find(|action| action.name == "Activate")
        .unwrap();
    assert_eq!(activate.effect.len(), 1);
    match &activate.effect[0] {
        Effect::Schedule {
            action,
            delay_seconds,
        } => {
            assert_eq!(action, "Refresh");
            assert_eq!(*delay_seconds, 2700);
        }
        other => panic!("expected Schedule, got: {other:?}"),
    }
}

#[test]
fn test_parse_counter_assignment_from_param() {
    let spec = r#"
[automaton]
name = "Upload"
states = ["Pending", "Ready"]
initial = "Pending"

[[state]]
name = "size_bytes"
type = "counter"
initial = "0"

[[action]]
name = "Complete"
from = ["Pending"]
to = "Ready"
effect = ["size_bytes = params.payload_size"]
"#;

    let automaton = parse_automaton(spec).expect("should parse a param assignment");
    let complete = automaton
        .actions
        .iter()
        .find(|action| action.name == "Complete")
        .unwrap();
    assert_eq!(complete.effect.len(), 1);
    assert_eq!(
        complete.effect[0].to_string(),
        "size_bytes = params.payload_size"
    );
}

#[test]
fn test_schedule_of_an_unknown_action_is_rejected() {
    let spec = r#"
[automaton]
name = "Broken"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
effect = ["schedule('Expire', 60)"]
"#;
    let err = parse_automaton(spec).expect_err("scheduling an undeclared action should fail");
    assert!(
        err.to_string()
            .contains("schedules unknown action 'Expire'"),
        "{err}"
    );
}

#[test]
fn test_field_invariant_section_does_not_overwrite_previous_action() {
    let spec = r#"
[automaton]
name = "Session"
states = ["Open", "Closed"]
initial = "Open"

[[action]]
name = "Archive"
kind = "input"
from = ["Open"]
to = "Closed"
hint = "Archive the session."

[[field_invariant]]
name = "ClosedRequiresArchivedAt"
message = "Closed sessions must set ArchivedAt"
assert = "Status == 'Closed' => ArchivedAt != null"
"#;

    let automaton = parse_automaton(spec).expect("should parse field invariants");
    let action_names: Vec<&str> = automaton
        .actions
        .iter()
        .map(|action| action.name.as_str())
        .collect();
    assert_eq!(action_names, vec!["Archive"]);
    assert_eq!(automaton.field_invariants.len(), 1);
    assert_eq!(
        automaton.field_invariants[0].name,
        "ClosedRequiresArchivedAt"
    );
}
