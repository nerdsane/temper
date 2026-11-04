//! Only a param whose value really is replaced by the record's own state is
//! refused: the other shapes reach fields, or read the value themselves.

use crate::automaton::parser::{LivenessEnforcement, parse_automaton_with_liveness};

/// Who is lost in a `Ticket` whose `Set` takes `flag` (declared with `kind`),
/// with `effect` and `guard` on `Set`, and `other` as `Other`'s effect.
fn lost(strict: bool, kind: &str, set: &str, other: &str) -> Vec<String> {
    let initial = match kind {
        "bool" => "false",
        "list" => "[]",
        "string" => "\"\"",
        _ => "0",
    };
    let param_type = match kind {
        "list" => "string",
        other => other,
    };
    let source = format!(
        r#"
[automaton]
name = "Ticket"
states = ["Open"]
initial = "Open"
strict_action_params = {strict}
allow_indefinite_states = ["Open"]

[[state]]
name = "flag"
type = "{kind}"
initial = {initial}

[[action]]
name = "Set"
kind = "input"
from = ["Open"]
to = "Open"
params = [{{ name = "flag", type = "{param_type}" }}]
{set}

[[action]]
name = "Other"
kind = "input"
from = ["Open"]
to = "Open"
{other}
"#
    );
    let automaton = parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly)
        .unwrap_or_else(|e| panic!("{e}\n{source}"));
    super::lint_lost_params(&automaton)
        .into_iter()
        .map(|finding| finding.message)
        .collect()
}

#[test]
fn a_strict_bool_or_counter_param_with_no_read_is_lost() {
    for kind in ["bool", "counter"] {
        let found = lost(true, kind, "", "");
        assert_eq!(found.len(), 1, "{kind}: {found:?}");
        for name in ["Set", "flag", "flag = params.flag"] {
            assert!(found[0].contains(name), "{}", found[0]);
        }
    }
}

#[test]
fn storing_or_reading_the_param_keeps_it() {
    assert!(lost(true, "bool", r#"effect = ["flag = params.flag"]"#, "").is_empty());
    assert!(lost(true, "bool", "guard = \"params.flag\"", "").is_empty());
}

#[test]
fn a_constraint_on_the_param_reads_it() {
    let compare =
        r#"constraints = [{ kind = "param_equals_field", param = "flag", field = "flag" }]"#;
    assert!(lost(true, "counter", compare, "").is_empty());
}

#[test]
fn an_effect_that_ignores_the_param_still_loses_it() {
    let found = lost(true, "counter", r#"effect = ["flag += 1"]"#, "");
    assert_eq!(found.len(), 1, "{found:?}");
}

#[test]
fn a_non_strict_record_holds_a_variable_only_once_written() {
    assert!(lost(false, "bool", "", "").is_empty());
    let found = lost(false, "bool", "", r#"effect = ["flag = true"]"#);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(lost(false, "list", "", "").is_empty());
    let found = lost(false, "list", "", r#"effect = ["append(flag, 'x')"]"#);
    assert_eq!(found.len(), 1, "{found:?}");
}

#[test]
fn a_strict_list_is_held_only_once_written() {
    assert!(lost(true, "list", "", "").is_empty());
}

#[test]
fn a_string_or_int_variable_does_not_replace_the_param() {
    assert!(lost(true, "string", "", "").is_empty());
    assert!(lost(true, "int", "", "").is_empty());
}
