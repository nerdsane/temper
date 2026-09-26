use super::*;

#[test]
fn extracts_declared_unique_keys() {
    // ADR-0153: [[key]] declares an alternate (unique) key the kernel indexes.
    let src = r#"
[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[key]]
name = "path"
properties = ["WorkspaceId", "Path"]

[[key]]
name = "id"
properties = ["Id"]
"#;
    let keys = parse_toml_to_automaton(src).expect("parse").keys;
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0].name, "path");
    assert_eq!(keys[0].properties, vec!["WorkspaceId", "Path"]);
    assert_eq!(keys[1].name, "id");
    assert_eq!(keys[1].properties, vec!["Id"]);
}

#[test]
fn extract_keys_empty_when_no_key_blocks() {
    let src = "[automaton]\nname = \"File\"\nstates = [\"Created\"]\ninitial = \"Created\"\n";
    assert!(parse_toml_to_automaton(src).expect("parse").keys.is_empty());
}

#[test]
fn extracts_declared_vector_paths() {
    // ADR-0155: [[vector]] declares a vector access path the kernel indexes.
    let src = r#"
[automaton]
name = "DesignLanguage"
states = ["Draft", "Published"]
initial = "Draft"

[[vector]]
name = "taste"
property = "taste_vector"
model_property = "taste_vector_model"
dims = 384
metric = "cosine"
"#;
    let vectors = parse_toml_to_automaton(src).expect("parse").vectors;
    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].name, "taste");
    assert_eq!(vectors[0].property, "taste_vector");
    assert_eq!(vectors[0].model_property, "taste_vector_model");
    assert_eq!(vectors[0].dims, 384);
    assert_eq!(vectors[0].metric, "cosine");
}

#[test]
fn extract_vectors_empty_when_no_vector_blocks() {
    let src = "[automaton]\nname = \"File\"\nstates = [\"Created\"]\ninitial = \"Created\"\n";
    assert!(
        parse_toml_to_automaton(src)
            .expect("parse")
            .vectors
            .is_empty()
    );
}

const MINIMAL_HEADER: &str = r#"
[automaton]
name = "Doc"
states = ["Draft", "Done"]
initial = "Draft"
"#;

fn parse_with(body: &str) -> Result<Automaton, AutomatonParseError> {
    parse_toml_to_automaton(&format!("{MINIMAL_HEADER}{body}"))
}

#[test]
fn invalid_toml_is_rejected() {
    let err = parse_with("[[action]]\nname = Finish\n").expect_err("unquoted string is not TOML");
    assert!(matches!(err, AutomatonParseError::Toml(_)), "{err}");
}

#[test]
fn lenient_core_values_are_accepted() {
    let auto = parse_with(
        r#"
[[state]]
name = "count"
type = "counter"
initial = 0
query_indexed = "false"

[[action]]
name = "Finish"
from = "Draft"
record_parent_event = "false"
"#,
    )
    .unwrap();
    assert_eq!(auto.actions[0].from, vec!["Draft"]);
    assert_eq!(auto.state[0].initial, "0");
    assert_eq!(auto.state[0].query_indexed, Some(false));
    assert!(!auto.actions[0].record_parent_event);
}

#[test]
fn guard_is_an_expression_and_effects_are_statements() {
    let auto = parse_with(
        r#"
[[action]]
name = "Finish"
guard = "ready && items >= 2"
effect = ["items += 1", "ready = true", "spawn('Child', 'Init')"]
"#,
    )
    .unwrap();
    let action = &auto.actions[0];
    assert_eq!(action.guard.to_string(), "ready && items >= 2");
    let effects: Vec<String> = action.effect.iter().map(ToString::to_string).collect();
    assert_eq!(
        effects,
        ["items += 1", "ready = true", "spawn('Child', 'Init')"]
    );
}

#[test]
fn old_effect_syntax_is_rejected_with_a_conversion_hint() {
    for effect in [
        "\"increment items\"",
        "[\"set ready true\"]",
        "[{ type = \"increment\", var = \"items\" }]",
    ] {
        let err = parse_with(&format!(
            "[[action]]\nname = \"Finish\"\neffect = {effect}\n"
        ))
        .expect_err("old effect syntax must not load");
        assert!(
            err.to_string().contains("migrate-predicates"),
            "{effect}: {err}"
        );
    }
}

#[test]
fn unknown_effect_is_rejected() {
    let err = parse_with("[[action]]\nname = \"Finish\"\neffect = [\"frobnicate(items)\"]\n")
        .expect_err("unknown effect must not be dropped");
    assert!(
        err.to_string().contains("unknown effect 'frobnicate'"),
        "{err}"
    );
}

#[test]
fn malformed_webhook_surfaces_error() {
    let err = parse_with("[[webhook]]\nname = \"cb\"\n").expect_err("webhook missing path/action");
    assert!(err.to_string().contains("webhook"), "{err}");
}

#[test]
fn context_entities_are_parsed() {
    let auto = parse_with(
        "[[context_entity]]\nname = \"parent\"\nentity_type = \"Lead\"\nid_field = \"lead_id\"\n",
    )
    .unwrap();
    assert_eq!(auto.context_entities.len(), 1);
    assert_eq!(auto.context_entities[0].entity_type, "Lead");
}

#[test]
fn parses_composite_action_metadata() {
    let input = r#"
[automaton]
name = "Repository"
states = ["Active"]
initial = "Active"

[[action]]
name = "IngestPack"
kind = "Composite"
from = ["Active"]
to = "Active"
params = ["PackBytes"]

[[action.cedar_gate]]
principal = "request.principal"
resource = "this"
action = "Repository::IngestPack"

[[action.sub_writes]]
target_entity = "Blob"
action = "Create"
generated_from = "pack_bytes"

[[action.sub_writes]]
target_entity = "Ref"
action = "Update"
generated_from = "ref_updates"
"#;

    let parsed = parse_toml_to_automaton(input).unwrap();
    let action = parsed
        .actions
        .iter()
        .find(|action| action.name == "IngestPack")
        .unwrap();

    assert_eq!(action.kind, "Composite");
    assert_eq!(
        action.cedar_gate.as_ref().map(|gate| gate.action.as_str()),
        Some("Repository::IngestPack")
    );
    assert_eq!(action.sub_writes.len(), 2);
    assert_eq!(action.sub_writes[0].target_entity, "Blob");
    assert_eq!(action.sub_writes[1].action, "Update");
}

// --- ADR-0049: [[state_timeout]] parsing --------------------------------

const SESSION_SPEC_WITH_TIMEOUTS: &str = r#"
[automaton]
name = "Session"
states = ["Created", "Provisioning", "Running", "Completed", "Failed", "WaitingForApproval"]
initial = "Created"
allow_indefinite_states = ["WaitingForApproval"]

[[action]]
name = "Configure"
from = ["Created"]
to = "Provisioning"

[[action]]
name = "TimeoutFail"
from = []
to = "Failed"
params = ["error_message"]

[[state_timeout]]
state = "Provisioning"
after_seconds = 180
on_timeout = "TimeoutFail"
reset_on = ["Heartbeat"]
params = { error_message = "provisioning did not complete within 180s" }

[[state_timeout]]
state = "Running"
after_seconds = 300
on_timeout = "TimeoutFail"
max_occurrences = 3
"#;

#[test]
fn state_timeout_parses_all_fields() {
    let auto = parse_toml_to_automaton(SESSION_SPEC_WITH_TIMEOUTS).unwrap();
    assert_eq!(auto.state_timeouts.len(), 2);

    let provisioning = &auto.state_timeouts[0];
    assert_eq!(provisioning.state, "Provisioning");
    assert_eq!(provisioning.after_seconds, 180);
    assert_eq!(provisioning.on_timeout, "TimeoutFail");
    assert_eq!(provisioning.max_occurrences, 1, "default should be 1");
    assert_eq!(provisioning.reset_on, vec!["Heartbeat".to_string()]);
    assert_eq!(
        provisioning.params.get("error_message").map(|s| s.as_str()),
        Some("provisioning did not complete within 180s")
    );
}

#[test]
fn state_timeout_max_occurrences_override() {
    let auto = parse_toml_to_automaton(SESSION_SPEC_WITH_TIMEOUTS).unwrap();
    let running = &auto.state_timeouts[1];
    assert_eq!(running.state, "Running");
    assert_eq!(running.max_occurrences, 3);
    assert!(
        running.reset_on.is_empty(),
        "reset_on omitted should default to empty"
    );
    assert!(running.params.is_empty());
}

#[test]
fn allow_indefinite_states_parses_from_automaton_block() {
    let auto = parse_toml_to_automaton(SESSION_SPEC_WITH_TIMEOUTS).unwrap();
    assert_eq!(
        auto.automaton.allow_indefinite_states,
        vec!["WaitingForApproval".to_string()]
    );
}

#[test]
fn absent_optional_sections_yield_defaults() {
    let minimal = r#"
[automaton]
name = "Trivial"
states = ["Idle"]
initial = "Idle"
"#;
    let auto = parse_toml_to_automaton(minimal).unwrap();
    assert!(auto.state_timeouts.is_empty());
    assert!(auto.automaton.allow_indefinite_states.is_empty());
    assert!(auto.admission.is_none());
}

#[test]
fn state_timeout_isolation_ignores_other_sections() {
    // Keys that share a name with state_timeout fields in other sections
    // must not leak into the timeout declarations.
    let spec = r#"
[automaton]
name = "X"
states = ["A", "B"]
initial = "A"

[[state]]
name = "state"
type = "string"
initial = "irrelevant"

[[action]]
name = "OnTimeout"
from = ["A"]
to = "B"
params = ["error_message"]

[[state_timeout]]
state = "A"
after_seconds = 10
on_timeout = "OnTimeout"
"#;
    let auto = parse_toml_to_automaton(spec).unwrap();
    assert_eq!(auto.state_timeouts.len(), 1);
    assert_eq!(auto.state_timeouts[0].state, "A");
}

#[test]
fn admission_block_parses_inline_action_map() {
    let spec = r#"
[automaton]
name = "X"
states = ["A"]
initial = "A"

[admission]
max_concurrent_creates = 5
max_concurrent_actions = { "Submit" = 3, "Configure" = 10 }
queue_depth = 75
queue_timeout_seconds = 20
"#;
    let auto = parse_toml_to_automaton(spec).unwrap();
    let admission = auto.admission.as_ref().expect("admission block parsed");
    assert_eq!(admission.max_concurrent_creates, Some(5));
    assert_eq!(
        admission.max_concurrent_actions.get("Submit").copied(),
        Some(3)
    );
    assert_eq!(
        admission.max_concurrent_actions.get("Configure").copied(),
        Some(10)
    );
    assert_eq!(admission.queue_depth, Some(75));
    assert_eq!(admission.queue_timeout_seconds, Some(20));
}

#[test]
fn state_timeout_malformed_surfaces_error() {
    // `after_seconds = "not a number"` should produce a serde error,
    // not a silent drop.
    let spec = r#"
[automaton]
name = "Bad"
states = ["A"]
initial = "A"

[[state_timeout]]
state = "A"
after_seconds = "not a number"
on_timeout = "X"
"#;
    let err = parse_toml_to_automaton(spec).expect_err("malformed after_seconds must surface");
    let msg = err.to_string();
    assert!(
        msg.contains("state_timeout"),
        "error should be scoped to state_timeout: {msg}"
    );
}

#[test]
fn old_predicate_syntax_fails_with_a_migration_hint() {
    let header = "[automaton]\nname = \"T\"\nstates = [\"A\", \"B\"]\ninitial = \"A\"\n\n[[state]]\nname = \"ready\"\ntype = \"bool\"\ninitial = \"false\"\n";
    for body in [
        "[[action]]\nname = \"Go\"\nfrom = [\"A\"]\nto = \"B\"\nguard = \"is_true ready\"\n",
        "[[action]]\nname = \"Go\"\nfrom = [\"A\"]\nto = \"B\"\nguard = [{ type = \"is_true\", var = \"ready\" }]\n",
        "[[invariant]]\nname = \"I\"\nwhen = [\"B\"]\nassert = \"ready\"\n",
        "[[invariant]]\nname = \"I\"\nassert = \"no_further_transitions\"\n",
        "[[field_invariant]]\nname = \"F\"\nwhen = { field = \"x\", absent = true }\nrequire = { field = \"y\", absent = true }\n",
    ] {
        let err = parse_toml_to_automaton(&format!("{header}{body}"))
            .and_then(|a| {
                crate::automaton::parse_automaton_with_liveness(
                    &format!("{header}{body}"),
                    crate::automaton::LivenessEnforcement::WarnOnly,
                )
                .map(|_| a)
            })
            .expect_err("old syntax must not load");
        assert!(
            err.to_string().contains("temper migrate-predicates"),
            "no hint for:\n{body}\n{err}"
        );
    }
}

#[test]
fn an_invariant_the_verifier_cannot_model_fails_to_load() {
    let spec = "[automaton]\nname = \"T\"\nstates = [\"A\"]\ninitial = \"A\"\n\n[[state]]\nname = \"title\"\ntype = \"string\"\ninitial = \"\"\n\n[[invariant]]\nname = \"HasTitle\"\nassert = \"title != ''\"\n";
    let err = crate::automaton::parse_automaton_with_liveness(
        spec,
        crate::automaton::LivenessEnforcement::WarnOnly,
    )
    .expect_err("unmodelable invariant must not load");
    assert!(err.to_string().contains("[[field_invariant]]"), "{err}");
}
