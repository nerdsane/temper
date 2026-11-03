//! Entry rules must cover a move from at least one declared place.

use crate::automaton::parser::{LivenessEnforcement, parse_automaton_with_liveness};

fn spec(states: &str, action_from: &str, rule_from: &str) -> String {
    format!(
        r#"
[automaton]
name = "Entry"
states = {states}
initial = "Inside"

[[action]]
name = "Enter"
{action_from}
to = "Inside"

[[entry_rule]]
name = "entry"
place = "Inside"
{rule_from}
assert = "false"
"#
    )
}

#[test]
fn wildcard_cannot_cover_a_default_entry_rule_in_a_single_place() {
    for action_from in ["", "from = []"] {
        let source = spec("['Inside']", action_from, "");
        let error = parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly)
            .expect_err("a default rule excludes its destination and covers no source")
            .to_string();
        assert!(error.contains("entry rule 'entry'"), "{error}");
        assert!(
            error.contains("no action moves into 'Inside' from a place it covers"),
            "{error}"
        );
    }
}

#[test]
fn wildcard_covers_a_default_entry_rule_from_another_declared_place() {
    for action_from in ["", "from = []"] {
        let source = spec("['Inside', 'Outside']", action_from, "");
        parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly)
            .expect("a wildcard move covers Outside");
    }
}

#[test]
fn wildcard_covers_an_explicit_self_entry_rule_in_a_single_place() {
    for action_from in ["", "from = []"] {
        let source = spec("['Inside']", action_from, "from = ['Inside']");
        parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly)
            .expect("an explicit rule may cover its destination");
    }
}

#[test]
fn a_move_from_only_the_destination_does_not_cover_a_default_entry_rule() {
    let source = spec("['Inside', 'Outside']", "from = ['Inside']", "");
    let error = parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly)
        .expect_err("the declared Outside place is not a source of this move")
        .to_string();
    assert!(
        error.contains("no action moves into 'Inside' from a place it covers"),
        "{error}"
    );
}

#[test]
fn entry_assertions_cannot_silently_read_unmodeled_numbers() {
    for assertion in [
        "amount > 0",
        "!(amount == 0)",
        "amount in [0, 1]",
        "true && amount != null",
    ] {
        let source = spec("['Inside', 'Outside']", "", "")
            .replace(
                "[[action]]",
                "[[state]]\nname = 'amount'\ntype = 'int'\ninitial = 0\n\n[[action]]",
            )
            .replace("assert = \"false\"", &format!("assert = \"{assertion}\""));
        let error = parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly)
            .expect_err("the verifier cannot evaluate an int read")
            .to_string();
        assert!(
            error.contains("entry rule 'entry'") && error.contains("cannot model"),
            "{error}"
        );
    }
}

#[test]
fn entry_assertions_cannot_silently_read_unmodeled_emptiness() {
    for assertion in [
        "!empty(reference)",
        "empty(reference)",
        "true && !empty(reference)",
        "false || !empty(reference)",
        "true => !empty(reference)",
        "!empty(reference) && Topping[reference].status == 'Stocked'",
    ] {
        let source = spec("['Inside', 'Outside']", "", "")
            .replace("assert = \"false\"", &format!("assert = \"{assertion}\""));
        let error = parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly)
            .expect_err("the verifier cannot evaluate emptiness of an undeclared field")
            .to_string();
        assert!(
            error.contains("entry rule 'entry'") && error.contains("cannot model"),
            "{error}"
        );
        assert!(error.contains("empty(reference)"), "{error}");
    }
}

#[test]
fn entry_assertions_can_read_undeclared_reference_statuses() {
    let source = spec("['Inside', 'Outside']", "", "").replace(
        "assert = \"false\"",
        "assert = \"Topping[reference].status != null\"",
    );
    parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly)
        .expect("a related status is modeled even when its reference field is undeclared");
}
