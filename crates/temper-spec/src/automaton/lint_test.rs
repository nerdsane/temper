use super::*;
use crate::automaton::parse_automaton;
use std::collections::BTreeMap;

#[test]
fn unknown_state_var_types_fail_to_parse() {
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
    let error = parse_automaton(src).expect_err("unknown type").to_string();
    assert!(error.contains("mystery_type"), "{error}");
}

#[test]
fn unknown_guard_and_effect_variables_fail_to_load() {
    let src = r#"
[automaton]
name = "Task"
states = ["Draft", "Done"]
initial = "Draft"

[[state]]
name = "approved"
type = "bool"
initial = false

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
guard = "phantom"
effect = ["ghost = true"]
"#;
    let err = parse_automaton(src).expect_err("unknown guard variable");
    assert!(
        err.to_string().contains("unknown state variable 'phantom'"),
        "{err}"
    );
    let err = parse_automaton(&src.replace("guard = \"phantom\"", "guard = \"approved\""))
        .expect_err("unknown effect variable");
    assert!(
        err.to_string().contains("unknown state variable 'ghost'"),
        "{err}"
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
"#;
    let automaton = parse_automaton(src).expect("parse");
    let findings = lint_automaton(&automaton);
    assert!(
        !findings
            .iter()
            .any(|finding| finding.code == "action_missing_to")
    );
}

const TIMED_JOB: &str = r#"
[automaton]
name = "Job"
states = ["Waiting", "Failed"]
initial = "Waiting"

[[action]]
name = "TimeoutFail"
from = ["Waiting"]
to = "Failed"

[[state_timeout]]
state = "Waiting"
after_seconds = 60
on_timeout = "TimeoutFail"
"#;

#[test]
fn max_occurrences_loads_and_is_reported_as_having_no_effect() {
    let src = format!("{TIMED_JOB}max_occurrences = 3\n");
    let findings = lint_automaton(&parse(&src));
    let finding = findings
        .iter()
        .find(|finding| finding.code == "state_timeout_max_occurrences_ignored")
        .expect("a set max_occurrences is reported");
    assert_eq!(finding.severity, LintSeverity::Warning);
    assert!(finding.message.contains("'Waiting'"), "{}", finding.message);
}

#[test]
fn a_timeout_without_max_occurrences_is_not_reported() {
    let findings = lint_automaton(&parse(TIMED_JOB));
    assert!(
        !findings
            .iter()
            .any(|finding| finding.code == "state_timeout_max_occurrences_ignored")
    );
}

#[test]
fn max_occurrences_zero_still_loads() {
    // It used to be rejected as an invalid budget; it is now a key with no
    // effect, so no value of it can make a spec fail to load.
    parse(&format!("{TIMED_JOB}max_occurrences = 0\n"));
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
effect = ["spawn('Task', 'Create')"]
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
effect = ["spawn('Task', 'Create')"]
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
effect = ["spawn('Task', 'Create')"]
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
effect = ["spawn('Task', 'Create')"]
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

/// A param the spawn leaves unset that names a declared variable reads as the
/// variable's initial value, so the spawn may leave it out; a param with no
/// declaration is still missing.
#[test]
fn bundle_lint_lets_a_declared_variable_cover_an_unmapped_spawn_param() {
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
effect = ["spawn('Task', 'Create')"]
"#,
    );
    let child = |declared: &str| {
        parse(&format!(
            r#"
[automaton]
name = "Task"
states = ["Open"]
initial = "Open"
{declared}
[[action]]
name = "Create"
from = ["Open"]
params = ["title", "description", "plan_id"]
"#
        ))
    };
    let unmapped = |task| {
        let bundle = BTreeMap::from([
            ("Plan".to_string(), parent.clone()),
            ("Task".to_string(), task),
        ]);
        lint_automata_bundle(&bundle)
            .into_iter()
            .filter(|finding| finding.code == "spawn_initial_action_params_unmapped")
            .collect::<Vec<_>>()
    };

    let declared = child(
        r#"
[[state]]
name = "description"
type = "string"
initial = ""
"#,
    );
    assert!(unmapped(declared).is_empty());
    let findings = unmapped(child(""));
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert!(findings[0].message.contains("description"), "{findings:?}");
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
effect = ["spawn('Task', 'Create')"]
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

#[test]
fn bundle_lint_maps_copied_fields_and_warns_on_unknown_copy_names() {
    let parent = parse(
        r#"
[automaton]
name = "Plan"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "Describe"
from = ["Draft"]
params = ["description"]

[[action]]
name = "AddTask"
from = ["Draft"]
params = ["title"]
effect = ["spawn('Task', 'Create', task_id, copy(description, descripton))"]
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
        !findings
            .iter()
            .any(|finding| finding.code == "spawn_initial_action_params_unmapped"),
        "a copied field supplies the child's param: {findings:?}"
    );
    assert!(findings.iter().any(|finding| {
        finding.code == "spawn_copy_field_unknown"
            && finding.severity == LintSeverity::Warning
            && finding.message.contains("'descripton'")
    }));
    assert!(
        !findings
            .iter()
            .any(|finding| finding.message.contains("'description' into")),
        "a name another action sets is not flagged"
    );
}

/// The `AdoptStatedRootCause` shape, reduced to its structure.
///
/// `Adopt` enters `Analyzed`, whose invariant asserts that `content_changed` is
/// clear -- and it satisfies that invariant by *writing* `false`, not by having
/// observed it. `Observe` is an input action legal in the same `Stale` state
/// that writes `content_changed` from what it actually saw. So a concurrent
/// observation's write is absorbed by a literal, and the entity lands in a
/// state whose invariant claims currency that nothing established.
const ABSORBING: &str = r#"
[automaton]
name = "Insight"
states = ["Pending", "Stale", "Analyzed"]
initial = "Pending"

[[state]]
name = "content_changed"
type = "bool"
initial = false

[[state]]
name = "has_verdict"
type = "bool"
initial = false

[[action]]
name = "Observe"
kind = "input"
from = ["Pending", "Stale"]
params = ["Changed"]
effect = ["content_changed = params.Changed"]

[[action]]
name = "Adopt"
kind = "input"
from = ["Pending", "Stale"]
to = "Analyzed"
effect = ["content_changed = false", "has_verdict = true"]

[[invariant]]
name = "AnalyzedMeansVerdictIsCurrent"
assert = "status in ['Analyzed'] => has_verdict && !content_changed"
"#;

#[test]
fn literal_write_absorbing_a_concurrent_observation_is_flagged() {
    let automaton = parse_automaton(ABSORBING).expect("parses");
    let findings = lint_automaton(&automaton);

    let finding = findings
        .iter()
        .find(|f| f.code == "absorbing_effect")
        .unwrap_or_else(|| panic!("expected an absorbing_effect finding, got: {findings:#?}"));

    // The message has to name all three parties, because the fix is a choice
    // between them: the absorbing action, the variable, and the writer whose
    // write is lost. Naming only the action sends the reader hunting.
    for needle in [
        "Adopt",
        "content_changed",
        "Observe",
        "AnalyzedMeansVerdictIsCurrent",
    ] {
        assert!(
            finding.message.contains(needle),
            "message should name {needle}: {}",
            finding.message
        );
    }
}

#[test]
fn a_literal_write_no_invariant_polices_is_not_flagged() {
    // Same collision on `content_changed`, but nothing in `Analyzed` asserts
    // anything about it, so overwriting it claims nothing and is just a write.
    let src = ABSORBING.replace(
        "assert = \"status in ['Analyzed'] => has_verdict && !content_changed\"",
        "assert = \"status in ['Analyzed'] => has_verdict\"",
    );
    let automaton = parse_automaton(&src).expect("parses");
    let findings = lint_automaton(&automaton);
    assert!(
        !findings.iter().any(|f| f.code == "absorbing_effect"),
        "no invariant reads content_changed in Analyzed: {findings:#?}"
    );
}

#[test]
fn a_write_derived_from_params_is_not_flagged() {
    // The remedy, asserted: carry the observation instead of asserting a
    // literal, and there is nothing to absorb.
    let src = ABSORBING.replace(
        "\"content_changed = false\"",
        "\"content_changed = params.Changed\"",
    );
    let src = src.replace(
        "from = [\"Pending\", \"Stale\"]\nto = \"Analyzed\"\neffect",
        "from = [\"Pending\", \"Stale\"]\nparams = [\"Changed\"]\nto = \"Analyzed\"\neffect",
    );
    let automaton = parse_automaton(&src).expect("parses");
    let findings = lint_automaton(&automaton);
    assert!(
        !findings.iter().any(|f| f.code == "absorbing_effect"),
        "a param-derived write absorbs nothing: {findings:#?}"
    );
}

#[test]
fn no_concurrent_writer_means_no_finding() {
    // The other remedy: forbid the racing writer where the claim is made.
    let src = ABSORBING.replace(
        "from = [\"Pending\", \"Stale\"]\nparams = [\"Changed\"]",
        "from = [\"Pending\"]\nparams = [\"Changed\"]",
    );
    let src = src.replace(
        "from = [\"Pending\", \"Stale\"]\nto = \"Analyzed\"",
        "from = [\"Stale\"]\nto = \"Analyzed\"",
    );
    let automaton = parse_automaton(&src).expect("parses");
    let findings = lint_automaton(&automaton);
    assert!(
        !findings.iter().any(|f| f.code == "absorbing_effect"),
        "Observe is not legal in Stale, so nothing races Adopt: {findings:#?}"
    );
}

#[test]
fn alternative_outcomes_of_one_operation_are_not_flagged() {
    // The success/failure callback pair, which is not a race: whichever fires
    // leaves `Stale`, so the other's precondition is gone and the two writes
    // cannot both land. Flagging these is the noise that would get this lint
    // switched off.
    let src = ABSORBING.replace(
        "from = [\"Pending\", \"Stale\"]\nparams = [\"Changed\"]\neffect",
        "from = [\"Pending\", \"Stale\"]\nto = \"Pending\"\nparams = [\"Changed\"]\neffect",
    );
    let automaton = parse_automaton(&src).expect("parses");
    let findings = lint_automaton(&automaton);
    // `Observe` now lands in `Pending`, which `Adopt` also fires from, so the
    // hazard survives -- assert the lint still sees it, then close the door.
    assert!(
        findings.iter().any(|f| f.code == "absorbing_effect"),
        "landing back in a state Adopt fires from is still a race: {findings:#?}"
    );

    let exits = ABSORBING.replace(
        "from = [\"Pending\", \"Stale\"]\nparams = [\"Changed\"]\neffect",
        "from = [\"Pending\", \"Stale\"]\nto = \"Analyzed\"\nparams = [\"Changed\"]\neffect",
    );
    let automaton = parse_automaton(&exits).expect("parses");
    let findings = lint_automaton(&automaton);
    assert!(
        !findings.iter().any(|f| f.code == "absorbing_effect"),
        "Observe exits every state Adopt fires from, so only one can apply: {findings:#?}"
    );
}

#[test]
fn guard_exclusion_prevents_absorbing_effect_warning() {
    let guarded = ABSORBING.replace(
        "name = \"Adopt\"",
        "name = \"Adopt\"\nguard = \"!has_verdict\"",
    );
    for (effect, should_warn) in [
        ("has_verdict = true", false),
        ("has_verdict = false", true),
        ("has_verdict = true\", \"has_verdict = params.Changed", true),
    ] {
        let source = guarded.replace(
            "content_changed = params.Changed",
            &format!("content_changed = params.Changed\", \"{effect}"),
        );
        let findings = lint_automaton(&parse_automaton(&source).unwrap());
        assert_eq!(
            findings.iter().any(|f| f.code == "absorbing_effect"),
            should_warn,
            "{source}\n{findings:#?}"
        );
    }
}
