use super::super::{LivenessEnforcement, parse_automaton_with_liveness};

const BASE_SPEC: &str = r#"
[automaton]
name = "SafetyContract"
states = ["Ready"]
initial = "Ready"
"#;

fn parse(source: &str) -> Result<super::super::Automaton, super::super::AutomatonParseError> {
    parse_automaton_with_liveness(source, LivenessEnforcement::WarnOnly)
}

fn assert_source_located_rejection(source: &str, expected: &str) {
    let error = parse(source).expect_err("malformed safety content must be rejected");
    let message = error.to_string();
    assert!(
        message.contains(expected),
        "error must identify `{expected}`; got: {message}"
    );
    assert!(
        message.contains("line") && message.contains("column"),
        "error must include a source location; got: {message}"
    );
}

#[test]
fn rejects_unknown_top_level_safety_table() {
    let source = format!(
        r#"{BASE_SPEC}
[[saftey]]
name = "status_is_valid"
assert = "status \\in {{Ready}}"
"#
    );

    assert_source_located_rejection(&source, "saftey");
}

#[test]
fn rejects_unknown_invariant_field_instead_of_keeping_an_empty_assertion() {
    let source = format!(
        r#"{BASE_SPEC}
[[invariant]]
name = "status_is_valid"
asser = "status \\in {{Ready}}"
"#
    );

    assert_source_located_rejection(&source, "asser");
}

#[test]
fn rejects_truncated_safety_assignment() {
    let source = format!(
        r#"{BASE_SPEC}
[[invariant]]
name = "status_is_valid"
assert =
"#
    );

    assert_source_located_rejection(&source, "TOML");
}

#[test]
fn rejects_unnamed_core_declarations() {
    let malformed_declarations = [
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

    for (kind, declaration) in malformed_declarations {
        let source = format!("{BASE_SPEC}{declaration}");
        let error = match parse(&source) {
            Ok(_) => panic!("unnamed {kind} declaration must be rejected"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(
            message.contains("name"),
            "unnamed {kind} error must identify the missing name; got: {message}"
        );
    }
}

#[test]
fn rejects_malformed_webhook_instead_of_silently_dropping_it() {
    let source = format!(
        r#"{BASE_SPEC}
[[webhook]]
name = "callback"
path = "callbacks/result"
method = "POST"
"#
    );

    assert_source_located_rejection(&source, "action");
}

#[test]
fn rejects_trailing_content_in_legacy_structured_effect() {
    let source = format!(
        r#"{BASE_SPEC}
[[action]]
name = "Complete"
from = ["Ready"]
to = "Ready"
effect = '''[{{ type = "emit", event = "completed" }}]
unknown = "discarded"'''
"#
    );

    assert_source_located_rejection(&source, "unknown");
}

#[test]
fn rejects_duplicate_safety_declaration_names() {
    let source = format!(
        r#"{BASE_SPEC}
[[invariant]]
name = "status_is_valid"
assert = "status \\in {{Ready}}"

[[invariant]]
name = "status_is_valid"
assert = "status \\in {{Ready}}"
"#
    );

    let error = parse(&source).expect_err("duplicate invariant names must be rejected");
    let message = error.to_string();
    assert!(message.contains("invariant"), "got: {message}");
    assert!(message.contains("status_is_valid"), "got: {message}");
}

#[test]
fn rejects_duplicate_safety_keys() {
    let source = format!(
        r#"{BASE_SPEC}
[[invariant]]
name = "status_is_valid"
assert = "status \\in {{Ready}}"
assert = "status \\in {{Ready}}"
"#
    );

    assert_source_located_rejection(&source, "duplicate key");
}

#[test]
fn retains_context_entities_across_canonical_round_trip() {
    let source = format!(
        r#"{BASE_SPEC}
[[context_entity]]
name = "workspace"
entity_type = "Workspace"
id_field = "workspace_id"
"#
    );

    let parsed = parse(&source).expect("valid context entity must parse");
    assert_eq!(parsed.context_entities.len(), 1);
    assert_eq!(parsed.context_entities[0].name, "workspace");

    let serialized = toml::to_string(&parsed).expect("canonical automaton must serialize");
    let reparsed = parse(&serialized).expect("serialized automaton must parse again");
    assert_eq!(reparsed.context_entities.len(), 1);
    assert_eq!(reparsed.context_entities[0].entity_type, "Workspace");
    assert_eq!(reparsed.context_entities[0].id_field, "workspace_id");
}
