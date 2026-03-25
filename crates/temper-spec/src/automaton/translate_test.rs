use super::*;
use crate::automaton::parse_automaton;

#[test]
fn translate_simple_action() {
    let spec = r#"
[automaton]
name = "Test"
states = ["Draft", "Active"]
initial = "Draft"

[[action]]
name = "Activate"
from = ["Draft"]
to = "Active"
"#;
    let automaton = parse_automaton(spec).unwrap();
    let actions = translate_actions(&automaton);
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].name, "Activate");
    assert_eq!(
        actions[0].guard,
        ResolvedGuard::StateIn(vec!["Draft".to_string()])
    );
    assert!(actions[0].effects.is_empty());
}

#[test]
fn translate_guards_combined() {
    let spec = r#"
[automaton]
name = "Test"
states = ["Draft", "Active"]
initial = "Draft"

[[state]]
name = "items"
type = "counter"
initial = "0"

[[action]]
name = "Submit"
from = ["Draft"]
to = "Active"
guard = [{ type = "min_count", var = "items", min = 1 }]
"#;
    let automaton = parse_automaton(spec).unwrap();
    let actions = translate_actions(&automaton);
    let action = &actions[0];
    match &action.guard {
        ResolvedGuard::And(guards) => {
            assert_eq!(guards.len(), 2);
            assert!(matches!(&guards[0], ResolvedGuard::StateIn(_)));
            assert!(matches!(
                &guards[1],
                ResolvedGuard::CounterMin { var, min: 1 } if var == "items"
            ));
        }
        _ => panic!("expected And guard, got {:?}", action.guard),
    }
}

#[test]
fn translate_effects_explicit() {
    let spec = r#"
[automaton]
name = "Test"
states = ["Draft", "Active"]
initial = "Draft"

[[action]]
name = "DoSomething"
from = ["Draft"]
to = "Active"
effect = [{ type = "increment", var = "count" }, { type = "set_bool", var = "done", value = true }, { type = "emit", event = "thing_done" }]
"#;
    let automaton = parse_automaton(spec).unwrap();
    let actions = translate_actions(&automaton);
    let effects = &actions[0].effects;
    assert_eq!(effects.len(), 3);
    assert!(matches!(&effects[0], ResolvedEffect::IncrementCounter(v) if v == "count"));
    assert!(matches!(
        &effects[1],
        ResolvedEffect::SetBool { var, value: true } if var == "done"
    ));
    assert!(matches!(&effects[2], ResolvedEffect::Emit(e) if e == "thing_done"));
}

#[test]
fn translate_name_heuristic_additem() {
    let spec = r#"
[automaton]
name = "Test"
states = ["Draft"]
initial = "Draft"

[[state]]
name = "items"
type = "counter"
initial = "0"

[[state]]
name = "quantity"
type = "counter"
initial = "0"

[[action]]
name = "AddItem"
from = ["Draft"]
"#;
    let automaton = parse_automaton(spec).unwrap();
    let actions = translate_actions(&automaton);
    let effects = &actions[0].effects;
    assert!(effects.len() >= 2);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, ResolvedEffect::IncrementCounter(v) if v == "items"))
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, ResolvedEffect::IncrementCounter(v) if v == "quantity"))
    );
}

#[test]
fn translate_runtime_only_effects() {
    let spec = r#"
[automaton]
name = "Test"
states = ["Idle", "Active"]
initial = "Idle"

[[action]]
name = "Start"
from = ["Idle"]
to = "Active"
effect = [{ type = "trigger", name = "run_wasm" }, { type = "schedule", action = "Refresh", delay_seconds = 60 }, { type = "spawn", entity_type = "Child", entity_id_source = "{uuid}", initial_action = "Init" }]
"#;
    let automaton = parse_automaton(spec).unwrap();
    let actions = translate_actions(&automaton);
    let effects = &actions[0].effects;
    assert_eq!(effects.len(), 3);
    assert!(!effects[0].is_verifiable());
    assert!(!effects[1].is_verifiable());
    assert!(!effects[2].is_verifiable());
}

#[test]
fn translate_cross_entity_guard() {
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
    let automaton = parse_automaton(spec).unwrap();
    let actions = translate_actions(&automaton);
    let action = &actions[0];
    match &action.guard {
        ResolvedGuard::And(guards) => {
            let has_cross = guards.iter().any(|guard| {
                matches!(guard, ResolvedGuard::CrossEntityState { entity_type, .. } if entity_type == "Child")
            });
            assert!(has_cross, "expected CrossEntityState guard");
        }
        _ => panic!("expected And guard"),
    }
}

#[test]
fn output_actions_filtered() {
    let spec = r#"
[automaton]
name = "Test"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "Notify"
kind = "output"

[[action]]
name = "DoWork"
from = ["Draft"]
"#;
    let automaton = parse_automaton(spec).unwrap();
    let actions = translate_actions(&automaton);
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].name, "DoWork");
}

#[test]
fn is_verifiable_classification() {
    assert!(ResolvedEffect::IncrementCounter("x".into()).is_verifiable());
    assert!(ResolvedEffect::DecrementCounter("x".into()).is_verifiable());
    assert!(
        ResolvedEffect::SetBool {
            var: "x".into(),
            value: true
        }
        .is_verifiable()
    );
    assert!(ResolvedEffect::ListAppend("x".into()).is_verifiable());
    assert!(ResolvedEffect::ListRemoveAt("x".into()).is_verifiable());
    assert!(!ResolvedEffect::Emit("e".into()).is_verifiable());
    assert!(!ResolvedEffect::Trigger("t".into()).is_verifiable());
    assert!(
        !ResolvedEffect::Schedule {
            action: "a".into(),
            delay_seconds: 1
        }
        .is_verifiable()
    );
    assert!(
        !ResolvedEffect::Spawn {
            entity_type: "T".into(),
            entity_id_source: "s".into(),
            initial_action: None,
            store_id_in: None,
        }
        .is_verifiable()
    );
}
