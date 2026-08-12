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
fn test_parse_set_counter_from_param_effect() {
    let spec = r#"
[automaton]
name = "Upload"
states = ["Pending", "Ready"]
initial = "Pending"

[[action]]
name = "Complete"
from = ["Pending"]
to = "Ready"
effect = [{ type = "set_counter_from_param", var = "size_bytes", param = "payload_size" }]
"#;

    let automaton = parse_automaton(spec).expect("should parse set_counter_from_param effect");
    let complete = automaton
        .actions
        .iter()
        .find(|action| action.name == "Complete")
        .unwrap();
    assert_eq!(complete.effect.len(), 1);
    match &complete.effect[0] {
        Effect::SetCounterFromParam { var, param } => {
            assert_eq!(var, "size_bytes");
            assert_eq!(param, "payload_size");
        }
        other => panic!("expected SetCounterFromParam, got: {other:?}"),
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
  { type = "spawn_entity", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create", copy_fields = "system_prompt, model, tools_enabled" },
  { type = "emit_event", event = "TaskAdded" }
]
"#;
    let automaton = parse_automaton(spec).expect("legacy aliases should parse");
    let add_task = automaton
        .actions
        .iter()
        .find(|action| action.name == "AddTask")
        .expect("AddTask action should exist");
    let Some(Effect::Spawn { copy_fields, .. }) = add_task.effect.first() else {
        panic!(
            "expected a spawn effect, got: {:?}",
            add_task.effect.first()
        );
    };
    assert_eq!(
        copy_fields
            .as_ref()
            .expect("legacy copy_fields should be preserved"),
        &["system_prompt", "model", "tools_enabled"].map(ToString::to_string)
    );
    assert!(matches!(add_task.effect.get(1), Some(Effect::Emit { .. })));
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
when = { field = "Status", equals = "Closed" }
require = { not = { field = "ArchivedAt", absent = true } }
message = "Closed sessions must set ArchivedAt"
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
