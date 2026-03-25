use super::*;
use crate::automaton::parse_automaton;
use std::collections::BTreeMap;

#[test]
fn lint_rejects_unknown_state_var_type() {
    let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[state]]
name = "mystery"
type = "mystery_type"
initial = "0"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
"#;
    let automaton = parse_automaton(src).expect("parse");
    let findings = lint_automaton(&automaton);
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "unknown_state_var_type"
                && finding.severity == LintSeverity::Error)
    );
}

#[test]
fn lint_rejects_unknown_guard_and_effect_variables() {
    let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[state]]
name = "approved"
type = "bool"
initial = "false"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
guard = "is_true phantom"
effect = "set ghost true"
"#;
    let automaton = parse_automaton(src).expect("parse");
    let findings = lint_automaton(&automaton);
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "guard_unknown_var"
                && finding.severity == LintSeverity::Error)
    );
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "effect_unknown_var"
                && finding.severity == LintSeverity::Error)
    );
}

#[test]
fn lint_warns_for_missing_to_on_internal_action() {
    let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "Nop"
kind = "internal"
from = ["Draft"]
"#;
    let automaton = parse_automaton(src).expect("parse");
    let findings = lint_automaton(&automaton);
    assert!(
        findings
            .iter()
            .any(|finding| finding.code == "action_missing_to"
                && finding.severity == LintSeverity::Warning)
    );
}

#[test]
fn lint_allows_missing_to_for_output_action() {
    let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "EmitAudit"
kind = "output"
from = ["Draft"]
effect = "emit audit"
"#;
    let automaton = parse_automaton(src).expect("parse");
    let findings = lint_automaton(&automaton);
    assert!(
        !findings
            .iter()
            .any(|finding| finding.code == "action_missing_to")
    );
}

fn parse(src: &str) -> Automaton {
    parse_automaton(src).expect("parse")
}

#[test]
fn bundle_lint_rejects_missing_spawn_target() {
    let parent = parse(
        r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "AddTask"
from = ["Draft"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
    );

    let bundle = BTreeMap::from([("Plan".to_string(), parent)]);
    let findings = lint_automata_bundle(&bundle);
    assert!(findings.iter().any(|finding| {
        finding.code == "spawn_target_missing"
            && finding.entity == "Plan"
            && finding.severity == LintSeverity::Error
    }));
}

#[test]
fn bundle_lint_rejects_missing_spawn_initial_action() {
    let parent = parse(
        r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "AddTask"
from = ["Draft"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
    );
    let child = parse(
        r#"
[automaton]
name = "Task"
states = ["Open", "Done"]
initial = "Open"

[[action]]
name = "Complete"
from = ["Open"]
to = "Done"
"#,
    );

    let bundle = BTreeMap::from([("Plan".to_string(), parent), ("Task".to_string(), child)]);
    let findings = lint_automata_bundle(&bundle);
    assert!(findings.iter().any(|finding| {
        finding.code == "spawn_initial_action_missing" && finding.entity == "Plan"
    }));
}

#[test]
fn bundle_lint_rejects_spawn_initial_action_not_enabled_from_initial() {
    let parent = parse(
        r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "AddTask"
from = ["Draft"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
    );
    let child = parse(
        r#"
[automaton]
name = "Task"
states = ["Open", "InProgress"]
initial = "Open"

[[action]]
name = "Create"
from = ["InProgress"]
"#,
    );

    let bundle = BTreeMap::from([("Plan".to_string(), parent), ("Task".to_string(), child)]);
    let findings = lint_automata_bundle(&bundle);
    assert!(
        findings
            .iter()
            .any(|finding| { finding.code == "spawn_initial_action_not_from_initial_state" })
    );
}

#[test]
fn bundle_lint_rejects_unmapped_spawn_params() {
    let parent = parse(
        r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "AddTask"
from = ["Draft"]
params = ["title"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
    );
    let child = parse(
        r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Create"
from = ["Open"]
params = ["title", "description", "plan_id"]
"#,
    );

    let bundle = BTreeMap::from([("Plan".to_string(), parent), ("Task".to_string(), child)]);
    let findings = lint_automata_bundle(&bundle);
    assert!(findings.iter().any(|finding| {
        finding.code == "spawn_initial_action_params_unmapped"
            && finding.entity == "Plan"
            && finding.message.contains("description")
    }));
}

#[test]
fn bundle_lint_accepts_valid_spawn_contract() {
    let parent = parse(
        r#"
[automaton]
name = "Plan"
states = ["Active"]
initial = "Active"

[[action]]
name = "AddTask"
from = ["Active"]
params = ["title", "description"]
effect = [{ type = "spawn", entity_type = "Task", entity_id_source = "{uuid}", initial_action = "Create" }]
"#,
    );
    let child = parse(
        r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"

[[action]]
name = "Create"
from = ["Open"]
params = ["title", "description", "plan_id"]
"#,
    );

    let bundle = BTreeMap::from([("Plan".to_string(), parent), ("Task".to_string(), child)]);
    let findings = lint_automata_bundle(&bundle);
    assert!(
        findings.is_empty(),
        "expected no bundle lint findings, got: {findings:?}"
    );
}
