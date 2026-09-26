use super::*;

#[test]
fn parse_minimal_automaton() {
    let toml_src = r#"
[automaton]
name = "Order"
states = ["Draft", "Active"]
initial = "Draft"
"#;
    let automaton: Automaton = toml::from_str(toml_src).unwrap();
    assert_eq!(automaton.automaton.name, "Order");
    assert_eq!(automaton.automaton.states, vec!["Draft", "Active"]);
    assert_eq!(automaton.automaton.initial, "Draft");
    assert!(automaton.actions.is_empty());
    assert!(automaton.invariants.is_empty());
    assert!(automaton.liveness.is_empty());
    assert!(automaton.integrations.is_empty());
}

#[test]
fn parse_action_defaults() {
    let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[action]]
name = "DoIt"
from = ["A"]
"#;
    let automaton: Automaton = toml::from_str(toml_src).unwrap();
    assert_eq!(automaton.actions.len(), 1);
    assert_eq!(automaton.actions[0].kind, "internal");
    assert!(automaton.actions[0].to.is_none());
    assert!(automaton.actions[0].guard.is_always());
    assert!(automaton.actions[0].effect.is_empty());
}

#[test]
fn parse_guard_variants() {
    let toml_src = r#"
[automaton]
name = "T"
states = ["A", "B"]
initial = "A"

[[action]]
name = "G1"
from = ["A"]
to = "B"
guard = "items >= 1 && items < 10 && ready && 'vip' in tags && len(tags) >= 2"
"#;
    let automaton: Automaton = toml::from_str(toml_src).unwrap();
    assert_eq!(
        automaton.actions[0].guard.to_string(),
        "items >= 1 && items < 10 && ready && 'vip' in tags && len(tags) >= 2"
    );
}

#[test]
fn parse_effect_variants() {
    let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[action]]
name = "E1"
from = ["A"]
effect = [
    { type = "increment", var = "count" },
    { type = "decrement", var = "count" },
    { type = "set_counter_from_param", var = "size_bytes", param = "payload_size" },
    { type = "increment", var = "bytes", amount = "size_bytes" },
    { type = "set_bool", var = "done", value = true },
    { type = "emit", event = "order_placed" },
    { type = "list_append", var = "log" },
    { type = "list_remove_at", var = "log" },
    { type = "trigger", name = "run_wasm" },
    { type = "schedule", action = "Retry", delay_seconds = 30 },
]
"#;
    let automaton: Automaton = toml::from_str(toml_src).unwrap();
    let effects = &automaton.actions[0].effect;
    assert_eq!(effects.len(), 10);
    assert!(matches!(&effects[0], Effect::Increment { var, amount: None } if var == "count"));
    assert!(matches!(&effects[1], Effect::Decrement { var, amount: None } if var == "count"));
    assert!(matches!(
        &effects[2],
        Effect::SetCounterFromParam { var, param } if var == "size_bytes" && param == "payload_size"
    ));
    assert!(matches!(
        &effects[3],
        Effect::Increment { var, amount: Some(amount) } if var == "bytes" && amount == "size_bytes"
    ));
    assert!(matches!(&effects[4], Effect::SetBool { var, value: true } if var == "done"));
    assert!(matches!(&effects[5], Effect::Emit { event } if event == "order_placed"));
    assert!(matches!(&effects[6], Effect::ListAppend { var } if var == "log"));
    assert!(matches!(&effects[7], Effect::ListRemoveAt { var } if var == "log"));
    assert!(matches!(&effects[8], Effect::Trigger { name } if name == "run_wasm"));
    assert!(
        matches!(&effects[9], Effect::Schedule { action, delay_seconds: 30 } if action == "Retry")
    );
}

#[test]
fn parse_spawn_effect() {
    let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[action]]
name = "S1"
from = ["A"]
effect = [
    { type = "spawn", entity_type = "Child", entity_id_source = "{uuid}", initial_action = "Init", store_id_in = "child_id" },
]
"#;
    let automaton: Automaton = toml::from_str(toml_src).unwrap();
    match &automaton.actions[0].effect[0] {
        Effect::Spawn {
            entity_type,
            entity_id_source,
            initial_action,
            store_id_in,
            copy_fields,
        } => {
            assert_eq!(entity_type, "Child");
            assert_eq!(entity_id_source, "{uuid}");
            assert_eq!(initial_action.as_deref(), Some("Init"));
            assert_eq!(store_id_in.as_deref(), Some("child_id"));
            assert!(copy_fields.is_none());
        }
        other => panic!("expected Spawn, got {other:?}"),
    }
}

#[test]
fn parse_invariant_and_liveness() {
    let toml_src = r#"
[automaton]
name = "T"
states = ["A", "B", "C"]
initial = "A"

[[invariant]]
name = "NonNeg"
assert = "status in ['B'] => count >= 0"

[[liveness]]
name = "Progress"
from = ["A"]
reaches = ["C"]
"#;
    let automaton: Automaton = toml::from_str(toml_src).unwrap();
    assert_eq!(automaton.invariants.len(), 1);
    assert_eq!(automaton.invariants[0].name, "NonNeg");
    assert_eq!(
        automaton.invariants[0].assert.to_string(),
        "status in ['B'] => count >= 0"
    );

    assert_eq!(automaton.liveness.len(), 1);
    assert_eq!(automaton.liveness[0].name, "Progress");
    assert_eq!(automaton.liveness[0].from, vec!["A"]);
    assert_eq!(automaton.liveness[0].reaches, vec!["C"]);
}

#[test]
fn parse_integration() {
    let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[integration]]
name = "payment"
trigger = "ChargeCard"
type = "wasm"
module = "payment_processor"
on_success = "PaymentConfirmed"
on_failure = "PaymentFailed"
"#;
    let automaton: Automaton = toml::from_str(toml_src).unwrap();
    assert_eq!(automaton.integrations.len(), 1);
    assert_eq!(automaton.integrations[0].name, "payment");
    assert_eq!(automaton.integrations[0].integration_type, "wasm");
    assert_eq!(
        automaton.integrations[0].module.as_deref(),
        Some("payment_processor")
    );
    assert_eq!(
        automaton.integrations[0].on_success.as_deref(),
        Some("PaymentConfirmed")
    );
}

#[test]
fn parse_webhook() {
    let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[webhook]]
name = "oauth_cb"
path = "oauth/callback"
action = "HandleCallback"
entity_param = "state"
"#;
    let automaton: Automaton = toml::from_str(toml_src).unwrap();
    assert_eq!(automaton.webhooks.len(), 1);
    assert_eq!(automaton.webhooks[0].name, "oauth_cb");
    assert_eq!(automaton.webhooks[0].method, "POST");
    assert_eq!(automaton.webhooks[0].entity_lookup, "query_param");
}

#[test]
fn parse_cross_entity_guard() {
    let toml_src = r#"
[automaton]
name = "T"
states = ["A", "B"]
initial = "A"

[[action]]
name = "Act"
from = ["A"]
to = "B"
guard = "empty(parent_id) || Parent[parent_id].status in ['Done', 'Approved']"
"#;
    let automaton = super::super::parse_automaton_with_liveness(
        toml_src,
        super::super::LivenessEnforcement::WarnOnly,
    )
    .unwrap();
    assert_eq!(
        automaton.actions[0].guard.to_string(),
        "empty(parent_id) || Parent[parent_id].status in ['Done', 'Approved']"
    );
}

#[test]
fn parse_cross_entity_guard_required_attribute() {
    let toml_src = r#"
[automaton]
name = "T"
states = ["A", "B"]
initial = "A"

[[action]]
name = "Act"
from = ["A"]
to = "B"
guard = "Parent[parent_id].status in ['Done']"
"#;
    let automaton = super::super::parse_automaton_with_liveness(
        toml_src,
        super::super::LivenessEnforcement::WarnOnly,
    )
    .unwrap();
    assert_eq!(
        automaton.actions[0].guard.to_string(),
        "Parent[parent_id].status in ['Done']"
    );
}

#[test]
fn parse_cross_entity_guard_forbidden_status() {
    let toml_src = r#"
[automaton]
name = "T"
states = ["A", "B"]
initial = "A"

[[action]]
name = "Act"
from = ["A"]
to = "B"
guard = "Workspace[workspace_id].status not in ['Frozen', 'Archived']"
"#;
    let automaton = super::super::parse_automaton_with_liveness(
        toml_src,
        super::super::LivenessEnforcement::WarnOnly,
    )
    .unwrap();
    assert_eq!(
        automaton.actions[0].guard.to_string(),
        "Workspace[workspace_id].status not in ['Frozen', 'Archived']"
    );
}

#[test]
fn parse_context_entity() {
    let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[context_entity]]
name = "parent"
entity_type = "ParentEntity"
id_field = "parent_id"
"#;
    let automaton: Automaton = toml::from_str(toml_src).unwrap();
    assert_eq!(automaton.context_entities.len(), 1);
    assert_eq!(automaton.context_entities[0].name, "parent");
    assert_eq!(automaton.context_entities[0].entity_type, "ParentEntity");
    assert_eq!(automaton.context_entities[0].id_field, "parent_id");
}

#[test]
fn state_var_parsing() {
    let toml_src = r#"
[automaton]
name = "T"
states = ["A"]
initial = "A"

[[state]]
name = "count"
type = "counter"
initial = "0"

[[state]]
name = "ready"
type = "bool"
initial = "false"
"#;
    let automaton: Automaton = toml::from_str(toml_src).unwrap();
    assert_eq!(automaton.state.len(), 2);
    assert_eq!(automaton.state[0].name, "count");
    assert_eq!(automaton.state[0].var_type, "counter");
    assert_eq!(automaton.state[1].var_type, "bool");
    assert_eq!(automaton.state[1].initial, "false");
}
