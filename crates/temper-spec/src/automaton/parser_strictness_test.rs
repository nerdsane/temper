//! Fail-closed regressions for the ADR-0171 schema boundary.

use super::super::{
    Automaton, AutomatonParseError, LivenessEnforcement, parse_automaton_with_liveness,
};

const BASE: &str = r#"
[automaton]
name = "SafetyContract"
states = ["Ready"]
initial = "Ready"
"#;

fn parse(source: &str) -> Result<Automaton, AutomatonParseError> {
    parse_automaton_with_liveness(source, LivenessEnforcement::WarnOnly)
}

fn must_reject_with(source: &str, expected: &str) {
    let message = parse(source)
        .expect_err("unknown or malformed schema content must fail closed")
        .to_string();
    assert!(
        message.contains(expected),
        "expected `{expected}` in: {message}"
    );
}

#[test]
fn rejects_unknown_top_level_table_and_nested_fields() {
    let cases = [
        (format!("{BASE}\n[[saftey]]\nname = \"valid\"\n"), "saftey"),
        (
            format!(
                "{BASE}\n[[action]]\nname = \"Stay\"\nfrom = [\"Ready\"]\nto = \"Ready\"\nparmz = []\n"
            ),
            "parmz",
        ),
        (
            format!(
                "{BASE}\n[[action]]\nname = \"Stay\"\nfrom = [\"Ready\"]\nto = \"Ready\"\nguard = [{{ type = \"state_in\", values = [\"Ready\"], typo = true }}]\n"
            ),
            "typo",
        ),
        (
            format!(
                "{BASE}\n[[action]]\nname = \"Stay\"\nfrom = [\"Ready\"]\nto = \"Ready\"\neffect = [{{ type = \"emit\", event = \"stayed\", typo = true }}]\n"
            ),
            "typo",
        ),
    ];
    for (source, expected) in cases {
        must_reject_with(&source, expected);
    }
}

#[test]
fn rejects_incomplete_and_duplicate_declarations() {
    must_reject_with(
        &format!("{BASE}\n[[action]]\nfrom = [\"Ready\"]\nto = \"Ready\"\n"),
        "name",
    );
    must_reject_with(
        &format!(
            "{BASE}\n[[invariant]]\nname = \"valid\"\nassert = \"status \\\\in {{Ready}}\"\n[[invariant]]\nname = \"valid\"\nassert = \"status \\\\in {{Ready}}\"\n"
        ),
        "declared twice",
    );
}

#[test]
fn rejects_malformed_webhook_missing_action() {
    must_reject_with(
        &format!(
            "{BASE}\n[[webhook]]\nname = \"callback\"\npath = \"callbacks/result\"\nmethod = \"POST\"\n"
        ),
        "action",
    );
}

#[test]
fn accepts_legacy_guard_and_effect_strings() {
    let source = format!(
        "{BASE}\n[[state]]\nname = \"enabled\"\ntype = \"bool\"\ninitial = \"true\"\n[[action]]\nname = \"Stay\"\nfrom = [\"Ready\"]\nto = \"Ready\"\nguard = \"is_true enabled\"\neffect = \"emit stayed\"\n"
    );
    let parsed = parse(&source).expect("documented legacy forms remain supported");
    assert_eq!(parsed.actions[0].guard.len(), 1);
    assert_eq!(parsed.actions[0].effect.len(), 1);
}

#[test]
fn accepts_legacy_structured_counter_names() {
    let source = format!(
        "{BASE}\n[[action]]\nname = \"Stay\"\nfrom = [\"Ready\"]\nto = \"Ready\"\n[[action.guard]]\ntype = \"CounterMin\"\nvar = \"count\"\nmin = 1\n[[action.effect]]\ntype = \"IncrementCounter\"\nvar = \"count\"\n"
    );
    let parsed = parse(&source).expect("historical structured names remain supported");
    assert_eq!(parsed.actions[0].guard.len(), 1);
    assert_eq!(parsed.actions[0].effect.len(), 1);
}
