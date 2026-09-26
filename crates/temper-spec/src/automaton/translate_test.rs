use super::*;
use crate::automaton::parse_automaton;
use crate::predicate::Arg;

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
    assert_eq!(actions[0].from_states, vec!["Draft".to_string()]);
    assert!(actions[0].guard.is_always());
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
guard = "items >= 1"
"#;
    let automaton = parse_automaton(spec).unwrap();
    let actions = translate_actions(&automaton);
    let action = &actions[0];
    assert_eq!(action.from_states, vec!["Draft".to_string()]);
    assert_eq!(action.guard.to_string(), "items >= 1");
}

const VARS: &str = r#"
[automaton]
name = "Test"
states = ["Draft", "Active"]
initial = "Draft"

[[state]]
name = "count"
type = "counter"
initial = "0"

[[state]]
name = "limit"
type = "counter"
initial = "0"

[[state]]
name = "done"
type = "bool"
initial = "false"

[[state]]
name = "tags"
type = "list"
initial = "[]"
"#;

fn effects_of(action: &str) -> Vec<ResolvedEffect> {
    let automaton = parse_automaton(&format!("{VARS}\n{action}")).unwrap();
    translate_actions(&automaton).remove(0).effects
}

fn arg(source: &str) -> Arg {
    crate::predicate::parse_arg(source).unwrap()
}

#[test]
fn effects_resolve_against_the_variable_types() {
    let effects = effects_of(
        r#"
[[action]]
name = "DoSomething"
from = ["Draft"]
to = "Active"
effect = ["count += 1", "count -= params.n", "limit = count", "done = true", "append(tags, 'vip')", "remove_at(tags, params.i)"]
"#,
    );
    assert_eq!(
        effects,
        vec![
            ResolvedEffect::AddCounter {
                var: "count".into(),
                value: arg("1")
            },
            ResolvedEffect::SubCounter {
                var: "count".into(),
                value: arg("params.n")
            },
            ResolvedEffect::SetCounter {
                var: "limit".into(),
                value: arg("count")
            },
            ResolvedEffect::SetBool {
                var: "done".into(),
                value: arg("true")
            },
            ResolvedEffect::ListAppend {
                var: "tags".into(),
                value: arg("'vip'")
            },
            ResolvedEffect::ListRemoveAt {
                var: "tags".into(),
                index: arg("params.i")
            },
        ]
    );
    assert!(effects.iter().all(ResolvedEffect::is_state_effect));
}

#[test]
fn action_names_imply_no_effects() {
    let effects = effects_of(
        r#"
[[action]]
name = "AddItem"
from = ["Draft"]
"#,
    );
    assert!(effects.is_empty(), "{effects:?}");
}

#[test]
fn runtime_only_effects_follow_the_statements_then_dispatches() {
    let effects = effects_of(
        r#"
[[action]]
name = "Start"
from = ["Draft"]
to = "Active"
effect = ["schedule('Refresh', 60)", "spawn('Child', 'Init', child_id)", "count += 1"]

[[action.triggers]]
name = "run_wasm"
kind = "wasm"
module = "runner"

[[action.triggers]]
name = "callback"
kind = "hook"
hook = "DispatchCallback"

[[action.triggers]]
name = "notify_child"
kind = "entity"
target_entity = "Child"
target_action = "Init"
resolve_target = { type = "same_id" }

[[action]]
name = "Refresh"
from = ["Active"]
"#,
    );
    assert_eq!(
        effects,
        vec![
            ResolvedEffect::Schedule {
                action: "Refresh".into(),
                delay_seconds: 60
            },
            ResolvedEffect::Spawn {
                entity_type: "Child".into(),
                initial_action: "Init".into(),
                store_id_in: Some("child_id".into()),
                id: None,
            },
            ResolvedEffect::AddCounter {
                var: "count".into(),
                value: arg("1")
            },
            ResolvedEffect::Dispatch("__trigger__:Start:run_wasm".into()),
            ResolvedEffect::Dispatch("DispatchCallback".into()),
        ]
    );
    let state: Vec<bool> = effects
        .iter()
        .map(ResolvedEffect::is_state_effect)
        .collect();
    assert_eq!(state, [false, false, true, false, false]);
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
guard = "empty(child_id) || Child[child_id].status in ['Done']"
"#;
    let automaton = parse_automaton(spec).unwrap();
    let actions = translate_actions(&automaton);
    let action = &actions[0];
    assert_eq!(
        action.guard.to_string(),
        "empty(child_id) || Child[child_id].status in ['Done']"
    );
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
