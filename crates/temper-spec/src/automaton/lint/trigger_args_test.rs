//! A trigger or timer on a strict type passes only parameters its target
//! action declares: otherwise the kernel refuses every dispatch it makes.

use crate::automaton::parser::{LivenessEnforcement, parse_automaton_with_liveness};

fn parse(source: &str) -> Result<(), String> {
    parse_automaton_with_liveness(source, LivenessEnforcement::WarnOnly)
        .map(|automaton| {
            assert!(
                super::lint_trigger_target_args(&automaton, &automaton.automaton.name).is_empty()
            );
        })
        .map_err(|e| e.to_string())
}

/// A `Ticket` whose `Answer` retries itself, passing `args`; `Retry` declares
/// only `note`.
fn spec(strict: bool, args: &str) -> String {
    format!(
        r#"
[automaton]
name = "Ticket"
states = ["Open", "Stuck"]
initial = "Open"
strict_action_params = {strict}

[[action]]
name = "Answer"
kind = "input"
from = ["Stuck"]
params = ["note"]

[[action.triggers]]
name = "again"
kind = "entity"
target_entity = "Ticket"
target_action = "Retry"
[action.triggers.args]
{args}
[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Retry"
kind = "input"
from = ["Stuck"]
to = "Open"
params = ["note"]
"#
    )
}

#[track_caller]
fn refused(spec: &str, needles: &[&str]) {
    let automaton = parse_automaton_with_liveness(spec, LivenessEnforcement::WarnOnly).unwrap();
    let findings = crate::automaton::lint_automaton(&automaton);
    let error = findings
        .iter()
        .find(|f| f.code == "trigger_target_arg_undeclared")
        .expect("install lint must refuse undeclared args");
    assert_eq!(error.severity, super::super::LintSeverity::Error);
    for needle in needles {
        assert!(
            error.message.contains(needle),
            "`{needle}` not in: {error:?}"
        );
    }
}

#[test]
fn a_self_trigger_passing_an_undeclared_parameter_is_refused() {
    refused(
        &spec(true, "note = \"note\"\nmoved_via = \"'the line'\""),
        &[
            "trigger 'again' on action 'Answer'",
            "'moved_via'",
            "'Retry'",
        ],
    );
}

#[test]
fn a_self_trigger_passing_declared_parameters_loads() {
    parse(&spec(true, "note = \"note\"")).expect("declared args load");
}

#[test]
fn a_type_that_is_not_strict_accepts_any_parameter() {
    parse(&spec(false, "moved_via = \"'the line'\"")).expect("lenient type loads");
}

#[test]
fn a_timer_passing_an_undeclared_parameter_is_refused() {
    let timer = "\n[[state_timeout]]\nstate = \"Stuck\"\nafter_seconds = 60\non_timeout = \"Retry\"\n\
                 [state_timeout.args]\nwhy = \"'timed out'\"\n";
    refused(
        &(spec(true, "note = \"note\"") + timer),
        &["state_timeout for 'Stuck'", "'why'", "'Retry'"],
    );
}

#[test]
fn a_spec_with_undeclared_args_remains_parseable_for_boot() {
    let source = spec(true, "moved_at = \"'now'\"\nmoved_via = \"'the line'\"");
    let restored = parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly)
        .expect("boot must keep loading an already installed spec");
    assert_eq!(restored.actions[0].triggers[0].args.len(), 2);
}

#[test]
fn a_renamed_spec_does_not_check_another_types_action() {
    let source = spec(true, "moved_at = \"'now'\"");
    let automaton = parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly).unwrap();
    assert!(
        crate::automaton::lint_automaton_for_type(&automaton, "ArchivedTicket")
            .iter()
            .all(|f| f.code != "trigger_target_arg_undeclared")
    );
    assert!(
        crate::automaton::lint_automaton_for_type(&automaton, "Ticket")
            .iter()
            .any(|f| f.code == "trigger_target_arg_undeclared")
    );
}

#[test]
fn a_trigger_targeting_its_registered_name_is_local_even_when_renamed() {
    let source = spec(true, "moved_at = \"'now'\"").replace(
        "target_entity = \"Ticket\"",
        "target_entity = \"ArchivedTicket\"",
    );
    let automaton = parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly).unwrap();
    assert!(
        crate::automaton::lint_automaton_for_type(&automaton, "ArchivedTicket")
            .iter()
            .any(|f| f.code == "trigger_target_arg_undeclared")
    );
}

#[test]
fn cross_type_targets_are_left_for_app_install_follow_up() {
    let source = spec(true, "moved_via = \"'the line'\"")
        .replace("target_entity = \"Ticket\"", "target_entity = \"Other\"");
    parse(&source).expect("cross-type targets are not checked per spec");
}

#[test]
fn undeclared_targets_are_left_to_the_kernel() {
    let source = spec(true, "moved_via = \"'the line'\"")
        .replace("target_action = \"Retry\"", "target_action = \"Carry\"");
    parse(&source).expect("carry moves are not declared actions");
}

#[test]
fn a_timer_passing_declared_parameters_loads() {
    let source = spec(true, "note = \"note\"")
        + "\n[[state_timeout]]\nstate = 'Stuck'\nafter_seconds = 60\non_timeout = 'Retry'\n[state_timeout.args]\nnote = \"'timed out'\"\n";
    parse(&source).expect("declared timer arg loads");
}

#[test]
fn a_renamed_spec_still_checks_its_local_timer_args() {
    let source = spec(true, "note = \"note\"")
        + "\n[[state_timeout]]\nstate = 'Stuck'\nafter_seconds = 60\non_timeout = 'Retry'\n[state_timeout.args]\nwhy = \"'timed out'\"\n";
    let automaton = parse_automaton_with_liveness(&source, LivenessEnforcement::WarnOnly).unwrap();
    assert!(
        crate::automaton::lint_automaton_for_type(&automaton, "ArchivedTicket")
            .iter()
            .any(|f| f.code == "trigger_target_arg_undeclared"
                && f.message.contains("state_timeout"))
    );
}
