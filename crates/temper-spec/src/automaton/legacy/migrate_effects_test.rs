use crate::automaton::legacy::migrate_source;
use crate::automaton::{ResolvedEffect, dispatch_effects, parse_automaton};

const HEADER: &str = r#"
[automaton]
name = "T"
states = ["A", "B"]
initial = "A"

[[state]]
name = "items"
type = "counter"
initial = "0"

[[state]]
name = "used"
type = "counter"
initial = "0"

[[state]]
name = "ready"
type = "bool"
initial = "false"

[[state]]
name = "tags"
type = "list"
initial = "[]"
"#;

fn migrate(body: &str) -> (String, Vec<String>) {
    let m = migrate_source(&format!("{HEADER}{body}")).unwrap_or_else(|e| panic!("{e}"));
    (m.source, m.notes)
}

fn effects(source: &str, action: &str) -> Vec<String> {
    let automaton = parse_automaton(source).unwrap();
    automaton
        .actions
        .iter()
        .find(|a| a.name == action)
        .unwrap()
        .effect
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn verb_and_table_effects_become_statements() {
    let (source, notes) = migrate(
        r#"
[[action]]
name = "Go"
from = ["A"]
to = "B"
effect = [
  "increment items",
  "decrement items",
  "increment used by size",
  "set ready true",
  { type = "set_counter_from_param", var = "used", param = "quota" },
  { type = "list_append", var = "tags" },
  { type = "list_remove_at", list = "tags" },
  { type = "schedule", action = "Back", delay_seconds = 30 },
  "schedule_at expires_at Back",
  { type = "spawn_entity", entity_type = "Child", entity_id_source = "{uuid}", initial_action = "Init", store_id_in = "child_id" },
  { type = "emit_event", event = "Gone" },
]

[[action]]
name = "Back"
from = ["B"]
to = "A"
"#,
    );
    assert_eq!(
        effects(&source, "Go"),
        [
            "items += 1",
            "items -= 1",
            "used += params.size",
            "ready = true",
            "used = params.quota",
            "append(tags, params.tags)",
            "remove_at(tags, params.tags_index)",
            "schedule('Back', 30)",
            "schedule_at('Back', expires_at)",
            "spawn('Child', 'Init', child_id)",
        ]
    );
    assert!(
        notes.iter().any(|n| n.contains("dropped `emit Gone`")),
        "{notes:?}"
    );
    // Converting again changes nothing.
    assert_eq!(migrate_source(&source).unwrap().source, source);
}

#[test]
fn a_param_sourced_spawn_id_is_kept() {
    let (source, _) = migrate(
        r#"
[[action]]
name = "Go"
from = ["A"]
effect = [{ type = "spawn", entity_type = "Child", entity_id_source = "child_key", initial_action = "Init", store_id_in = "child_id" }]
"#,
    );
    assert_eq!(
        effects(&source, "Go"),
        ["spawn('Child', 'Init', child_id, params.child_key)"]
    );
}

#[test]
fn integrations_become_triggers_on_the_actions_that_fired_them() {
    let (source, notes) = migrate(
        r#"
[[action]]
name = "Charge"
from = ["A"]
to = "B"
effect = ["increment items", "trigger charge"]

[[action]]
name = "Done"
from = ["B"]

[[action]]
name = "Failed"
from = ["B"]

[[integration]]
name = "charge"
trigger = "charge"
type = "wasm"
module = "stripe"
on_success = "Done"
on_failure = "Failed"
url = "https://example.com/charge"

[[integration]]
name = "notify"
trigger = "Done"
type = "webhook"

[[integration]]
name = "unused"
trigger = "nobody"
type = "wasm"
module = "m"
"#,
    );
    assert!(!source.contains("[[integration]]"), "{source}");
    let automaton = parse_automaton(&source).unwrap();
    let charge = automaton
        .actions
        .iter()
        .find(|a| a.name == "Charge")
        .unwrap();
    assert_eq!(effects(&source, "Charge"), ["items += 1"]);
    let trigger = &charge.triggers[0];
    assert_eq!(trigger.name, "charge");
    assert_eq!(trigger.module.as_deref(), Some("stripe"));
    assert_eq!(trigger.on_success.as_deref(), Some("Done"));
    assert_eq!(trigger.on_failure.as_deref(), Some("Failed"));
    assert_eq!(
        trigger.config.get("url").map(String::as_str),
        Some("https://example.com/charge")
    );
    assert!(
        notes.iter().any(|n| n.contains("'notify' dropped")),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|n| n.contains("'unused' dropped")),
        "{notes:?}"
    );
}

#[test]
fn trigger_effects_resolve_to_local_shared_or_hook_triggers() {
    let (source, notes) = migrate(
        r#"
[[action]]
name = "Prepare"
from = ["A"]
to = "B"
effect = "trigger prepare"

[[action.triggers]]
name = "prepare"
kind = "wasm"
module = "preparer"

[action.triggers.config]
url = "https://example.com"

[[action]]
name = "Again"
from = ["B"]
effect = "trigger prepare"

[[action]]
name = "Approve"
from = ["B"]
to = "A"
effect = [{ type = "trigger", name = "DispatchCallback" }]
"#,
    );
    let automaton = parse_automaton(&source).unwrap();
    let dispatches =
        |name: &str| dispatch_effects(automaton.actions.iter().find(|a| a.name == name).unwrap());
    assert_eq!(
        dispatches("Prepare"),
        [ResolvedEffect::Dispatch(
            "__trigger__:Prepare:prepare".into()
        )]
    );
    assert_eq!(
        dispatches("Again"),
        [ResolvedEffect::Dispatch("__trigger__:Again:prepare".into())]
    );
    assert_eq!(
        dispatches("Approve"),
        [ResolvedEffect::Dispatch("DispatchCallback".into())]
    );
    let again = automaton
        .actions
        .iter()
        .find(|a| a.name == "Again")
        .unwrap();
    assert_eq!(
        again.triggers[0].config.get("url").map(String::as_str),
        Some("https://example.com")
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("copied trigger 'prepare' from action 'Prepare'")),
        "{notes:?}"
    );
}

#[test]
fn an_unresolvable_trigger_effect_is_an_error() {
    let err = migrate_source(&format!(
        "{HEADER}\n[[action]]\nname = \"Go\"\nfrom = [\"A\"]\neffect = \"trigger nowhere\"\n"
    ))
    .unwrap_err();
    assert!(err.contains("`trigger nowhere` matches no"), "{err}");
}

#[test]
fn item_named_actions_get_the_effects_their_name_implied() {
    let (source, notes) = migrate(
        r#"
[[action]]
name = "AddItem"
from = ["A"]

[[action]]
name = "RemoveItem"
from = ["A"]
guard = "items > 0"

[[action]]
name = "Submit"
from = ["A"]
to = "B"
"#,
    );
    assert_eq!(effects(&source, "AddItem"), ["items += 1", "used += 1"]);
    assert_eq!(effects(&source, "RemoveItem"), ["items -= 1", "used -= 1"]);
    assert!(effects(&source, "Submit").is_empty());
    assert_eq!(notes.len(), 2, "{notes:?}");
}
