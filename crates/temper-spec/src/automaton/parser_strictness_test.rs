//! Fail-closed safety regressions for ARN-214 / ADR-0179.
//!
//! The historical hand-rolled IOA parser ignored unknown lines/tables and
//! dropped incomplete blocks. These tests require the public parser to reject
//! that content with a source-located error.

use super::super::{LivenessEnforcement, parse_automaton_with_liveness};

const BASE: &str = r#"
[automaton]
name = "SafetyContract"
states = ["Ready"]
initial = "Ready"
"#;

fn parse(source: &str) -> Result<super::super::Automaton, super::super::AutomatonParseError> {
    parse_automaton_with_liveness(source, LivenessEnforcement::WarnOnly)
}

fn must_reject_with(source: &str, expected_fragment: &str) {
    let err = parse(source).expect_err("malformed or unknown safety content must fail closed");
    let message = err.to_string();
    assert!(
        message.contains(expected_fragment),
        "error must mention `{expected_fragment}`; got: {message}"
    );
    assert!(
        message.contains("line") && message.contains("column"),
        "error must include source span (line/column); got: {message}"
    );
}

#[test]
fn rejects_unknown_top_level_table_typo() {
    let source = format!(
        r#"{BASE}
[[saftey]]
name = "status_is_valid"
assert = "status \\in {{Ready}}"
"#
    );
    must_reject_with(&source, "saftey");
}

#[test]
fn rejects_unknown_invariant_field_typo() {
    let source = format!(
        r#"{BASE}
[[invariant]]
name = "status_is_valid"
asser = "status \\in {{Ready}}"
"#
    );
    must_reject_with(&source, "asser");
}

#[test]
fn rejects_truncated_invariant_assignment() {
    let source = format!(
        r#"{BASE}
[[invariant]]
name = "status_is_valid"
assert =
"#
    );
    must_reject_with(&source, "TOML");
}

#[test]
fn rejects_unnamed_action_invariant_and_liveness_blocks() {
    let cases = [
        (
            "action",
            r#"
[[action]]
from = ["Ready"]
to = "Ready"
"#,
        ),
        (
            "invariant",
            r#"
[[invariant]]
assert = "status \\in {Ready}"
"#,
        ),
        (
            "liveness",
            r#"
[[liveness]]
from = ["Ready"]
has_actions = true
"#,
        ),
    ];

    for (kind, block) in cases {
        let source = format!("{BASE}{block}");
        let err = match parse(&source) {
            Ok(_) => panic!("unnamed {kind} block must be rejected, not silently dropped"),
            Err(e) => e,
        };
        let message = err.to_string();
        assert!(
            message.contains("name"),
            "unnamed {kind} rejection must mention missing name; got: {message}"
        );
    }
}

#[test]
fn rejects_malformed_webhook_missing_action() {
    let source = format!(
        r#"{BASE}
[[webhook]]
name = "callback"
path = "callbacks/result"
method = "POST"
"#
    );
    must_reject_with(&source, "action");
}

#[test]
fn rejects_trailing_garbage_in_string_wrapped_effect_array() {
    let source = format!(
        r#"{BASE}
[[action]]
name = "Complete"
from = ["Ready"]
to = "Ready"
effect = '''[{{ type = "emit", event = "completed" }}]
unknown = "discarded"'''
"#
    );
    must_reject_with(&source, "unknown");
}

#[test]
fn rejects_duplicate_invariant_names() {
    let source = format!(
        r#"{BASE}
[[invariant]]
name = "status_is_valid"
assert = "status \\in {{Ready}}"

[[invariant]]
name = "status_is_valid"
assert = "status \\in {{Ready}}"
"#
    );
    let err = parse(&source).expect_err("duplicate invariant names must fail closed");
    let message = err.to_string();
    assert!(message.contains("invariant"), "got: {message}");
    assert!(message.contains("status_is_valid"), "got: {message}");
}

#[test]
fn rejects_duplicate_keys_inside_invariant_table() {
    let source = format!(
        r#"{BASE}
[[invariant]]
name = "status_is_valid"
assert = "status \\in {{Ready}}"
assert = "status \\in {{Ready}}"
"#
    );
    must_reject_with(&source, "duplicate key");
}

#[test]
fn retains_context_entity_through_serialize_parse_round_trip() {
    let source = format!(
        r#"{BASE}
[[context_entity]]
name = "workspace"
entity_type = "Workspace"
id_field = "workspace_id"
"#
    );

    let parsed = parse(&source).expect("valid context_entity must parse");
    assert_eq!(parsed.context_entities.len(), 1);
    assert_eq!(parsed.context_entities[0].name, "workspace");

    let serialized = toml::to_string(&parsed).expect("automaton must serialize");
    let again = parse(&serialized).expect("serialized form must reparse");
    assert_eq!(again.context_entities.len(), 1);
    assert_eq!(again.context_entities[0].entity_type, "Workspace");
    assert_eq!(again.context_entities[0].id_field, "workspace_id");
}
