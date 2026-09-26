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
    "count += 1",
    "count -= 1",
    "size_bytes = params.payload_size",
    "done = true",
    "append(log, params.entry)",
    "remove_at(log, 0)",
    "schedule('Retry', 30)",
    "spawn('Child', 'Init', child_id)",
]
"#;
    let automaton: Automaton = toml::from_str(toml_src).unwrap();
    let effects: Vec<String> = automaton.actions[0]
        .effect
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        effects,
        [
            "count += 1",
            "count -= 1",
            "size_bytes = params.payload_size",
            "done = true",
            "append(log, params.entry)",
            "remove_at(log, 0)",
            "schedule('Retry', 30)",
            "spawn('Child', 'Init', child_id)",
        ]
    );
    assert!(matches!(
        &automaton.actions[0].effect[7],
        Effect::Spawn { entity_type, initial_action, store_id_in: Some(field), id: None }
            if entity_type == "Child" && initial_action == "Init" && field == "child_id"
    ));
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
