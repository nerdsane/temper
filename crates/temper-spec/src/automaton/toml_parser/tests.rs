use super::inline::{parse_inline_fields, split_inline_tables};
use super::*;

#[test]
fn parse_kv_simple() {
    let (key, value) = parse_kv("name = \"Order\"").unwrap();
    assert_eq!(key, "name");
    assert_eq!(value, "Order");
}

#[test]
fn parse_kv_no_equals() {
    assert!(parse_kv("no_equals_here").is_none());
}

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
    let keys = extract_keys(src).expect("extract keys");
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0].name, "path");
    assert_eq!(keys[0].properties, vec!["WorkspaceId", "Path"]);
    assert_eq!(keys[1].name, "id");
    assert_eq!(keys[1].properties, vec!["Id"]);
}

#[test]
fn extract_keys_empty_when_no_key_blocks() {
    let src = "[automaton]\nname = \"File\"\nstates = [\"Created\"]\ninitial = \"Created\"\n";
    assert!(extract_keys(src).expect("extract keys").is_empty());
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
    let vectors = extract_vectors(src).expect("extract vectors");
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
    assert!(extract_vectors(src).expect("extract vectors").is_empty());
}

#[test]
fn parse_kv_trims_whitespace() {
    let (key, value) = parse_kv("  key  =  \"value\"  ").unwrap();
    assert_eq!(key, "key");
    assert_eq!(value, "value");
}

#[test]
fn parse_string_array_simple() {
    let arr = parse_string_array("[\"Draft\", \"Active\", \"Done\"]");
    assert_eq!(arr, vec!["Draft", "Active", "Done"]);
}

#[test]
fn parse_string_array_single_value() {
    let arr = parse_string_array("\"Active\"");
    assert_eq!(arr, vec!["Active"]);
}

#[test]
fn parse_string_array_empty_brackets() {
    let arr = parse_string_array("[]");
    assert!(arr.is_empty());
}

#[test]
fn split_inline_tables_two_items() {
    let result = split_inline_tables("{a = 1}, {b = 2}");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0], "{a = 1}");
    assert_eq!(result[1], "{b = 2}");
}

#[test]
fn split_inline_tables_empty() {
    let result = split_inline_tables("");
    assert!(result.is_empty());
}

#[test]
fn parse_inline_fields_simple() {
    let map = parse_inline_fields("type = \"schedule\", action = \"Refresh\"");
    assert_eq!(map.get("type").unwrap(), "schedule");
    assert_eq!(map.get("action").unwrap(), "Refresh");
}

#[test]
fn parse_inline_fields_keeps_nested_arrays_together() {
    let map = parse_inline_fields(
        "type = \"cross_entity_state\", required_status = [\"Draft\", \"Ready\"]",
    );
    assert_eq!(map.get("type").unwrap(), "cross_entity_state");
    assert_eq!(
        map.get("required_status").unwrap(),
        "[\"Draft\", \"Ready\"]"
    );
}

#[test]
fn parse_inline_fields_empty() {
    let map = parse_inline_fields("");
    assert!(map.is_empty());
}

#[test]
fn join_multiline_single_line() {
    let result = join_multiline_arrays("key = [\"a\", \"b\"]");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], "key = [\"a\", \"b\"]");
}

#[test]
fn join_multiline_continuation() {
    let input = "effect = [\n  { var = \"x\" },\n]";
    let result = join_multiline_arrays(input);
    assert_eq!(result.len(), 1);
    assert!(result[0].contains("effect = ["));
    assert!(result[0].contains(']'));
}

#[test]
fn join_multiline_no_brackets() {
    let input = "name = \"Test\"\ninitial = \"Draft\"";
    let result = join_multiline_arrays(input);
    assert_eq!(result.len(), 2);
}

#[test]
fn guard_gt() {
    let g = parse_guard_clause("items > 3").unwrap();
    assert!(matches!(g, Guard::MinCount { ref var, min: 4 } if var == "items"));
}

#[test]
fn guard_gte() {
    let g = parse_guard_clause("items >= 5").unwrap();
    assert!(matches!(g, Guard::MinCount { ref var, min: 5 } if var == "items"));
}

#[test]
fn guard_lt() {
    let g = parse_guard_clause("items < 10").unwrap();
    assert!(matches!(g, Guard::MaxCount { ref var, max: 10 } if var == "items"));
}

#[test]
fn guard_lte() {
    let g = parse_guard_clause("items <= 10").unwrap();
    assert!(matches!(g, Guard::MaxCount { ref var, max: 11 } if var == "items"));
}

#[test]
fn guard_prefix_min() {
    let g = parse_guard_clause("min items 3").unwrap();
    assert!(matches!(g, Guard::MinCount { ref var, min: 3 } if var == "items"));
}

#[test]
fn guard_prefix_max() {
    let g = parse_guard_clause("max items 10").unwrap();
    assert!(matches!(g, Guard::MaxCount { ref var, max: 10 } if var == "items"));
}

#[test]
fn guard_is_true() {
    let g = parse_guard_clause("is_true approved").unwrap();
    assert!(matches!(g, Guard::IsTrue { ref var } if var == "approved"));
}

#[test]
fn guard_list_contains() {
    let g = parse_guard_clause("list_contains tags vip").unwrap();
    assert!(
        matches!(g, Guard::ListContains { ref var, ref value } if var == "tags" && value == "vip")
    );
}

#[test]
fn guard_list_length_min() {
    let g = parse_guard_clause("list_length_min tags 2").unwrap();
    assert!(matches!(g, Guard::ListLengthMin { ref var, min: 2 } if var == "tags"));
}

#[test]
fn guard_bare_boolean() {
    let g = parse_guard_clause("has_mutation").unwrap();
    assert!(matches!(g, Guard::IsTrue { ref var } if var == "has_mutation"));
}

#[test]
fn guard_negation_prefix() {
    let g = parse_guard_clause("!needs_approval").unwrap();
    assert!(matches!(g, Guard::IsFalse { ref var } if var == "needs_approval"));
}

#[test]
fn guard_is_false_prefix() {
    let g = parse_guard_clause("is_false budget_exhausted").unwrap();
    assert!(matches!(g, Guard::IsFalse { ref var } if var == "budget_exhausted"));
}

#[test]
fn guard_unsupported_syntax() {
    assert!(parse_guard_clause("two words bad").is_err());
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
fn state_timeout_absent_yields_empty_vec() {
    let minimal = r#"
[automaton]
name = "Trivial"
states = ["Idle"]
initial = "Idle"
"#;
    let auto = parse_toml_to_automaton(minimal).unwrap();
    assert!(auto.state_timeouts.is_empty());
    assert!(auto.automaton.allow_indefinite_states.is_empty());
}

#[test]
fn state_timeout_isolation_ignores_other_sections() {
    // Ensures extract_state_timeouts' isolation doesn't pick up keys that
    // happen to share a name with state_timeout fields in other sections.
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

[[integration]]
name = "noop"
trigger = "noop"
type = "webhook"

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
fn admission_block_absent_yields_none() {
    let minimal = r#"
[automaton]
name = "Trivial"
states = ["Idle"]
initial = "Idle"
"#;
    let auto = parse_toml_to_automaton(minimal).unwrap();
    assert!(auto.admission.is_none());
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
