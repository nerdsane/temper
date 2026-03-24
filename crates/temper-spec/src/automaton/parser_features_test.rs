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
effect = "set is_done true"
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

[[action]]
name = "Queue"
from = ["Open"]
to = "Queued"
guard = "max retries 3"

[[action]]
name = "Escalate"
from = ["Queued"]
to = "Queued"
guard = "list_contains labels urgent"

[[action]]
name = "Close"
from = ["Queued"]
to = "Closed"
guard = "list_length_min labels 1"
"#;

    let automaton = parse_automaton(spec).expect("extended guard forms should parse");
    let queue = automaton
        .actions
        .iter()
        .find(|action| action.name == "Queue")
        .unwrap();
    assert!(matches!(
        queue.guard.as_slice(),
        [Guard::MaxCount { var, max }] if var == "retries" && *max == 3
    ));

    let escalate = automaton
        .actions
        .iter()
        .find(|action| action.name == "Escalate")
        .unwrap();
    assert!(matches!(
        escalate.guard.as_slice(),
        [Guard::ListContains { var, value }] if var == "labels" && value == "urgent"
    ));

    let close = automaton
        .actions
        .iter()
        .find(|action| action.name == "Close")
        .unwrap();
    assert!(matches!(
        close.guard.as_slice(),
        [Guard::ListLengthMin { var, min }] if var == "labels" && *min == 1
    ));
}

#[test]
fn test_invalid_guard_number_rejected() {
    let spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
from = ["Draft"]
to = "Submitted"
guard = "items > nope"
"#;

    let err = parse_automaton(spec).expect_err("invalid numeric guard should fail");
    assert!(err.to_string().contains("right side must be an integer"));
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
effect = [{ type = "schedule", action = "Refresh", delay_seconds = 2700 }]
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
fn test_unknown_inline_effect_type_rejected() {
    let spec = r#"
[automaton]
name = "Broken"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
effect = [{ type = "mystery_effect", value = "x" }]
"#;
    let err = parse_automaton(spec).expect_err("unknown inline effect type should fail");
    assert!(
        err.to_string()
            .contains("unsupported effect type 'mystery_effect'")
    );
}

#[test]
fn test_legacy_inline_effect_aliases_supported() {
    let spec = r#"
[automaton]
name = "Plan"
states = ["Active"]
initial = "Active"

[[action]]
name = "AddTask"
from = ["Active"]
effect = [
  { type = "spawn_entity", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" },
  { type = "emit_event", event = "TaskAdded" }
]
"#;
    let automaton = parse_automaton(spec).expect("legacy aliases should parse");
    let add_task = automaton
        .actions
        .iter()
        .find(|action| action.name == "AddTask")
        .expect("AddTask action should exist");
    assert!(matches!(
        add_task.effect.first(),
        Some(Effect::Spawn { .. })
    ));
    assert!(matches!(add_task.effect.get(1), Some(Effect::Emit { .. })));
}
