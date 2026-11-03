//! Effect literals participate in the same string equality classes as inputs.

use temper_verify::check_model;
use temper_verify::model::{ModelString, ParamValue, build_model_from_ioa};
use temper_verify::proptest_gen::run_prop_tests_on_model;
use temper_verify::simulation::{SimConfig, run_multi_seed_simulation};

const SOURCE: &str = r#"
[automaton]
name = "Entry"
states = ["Setup", "Waiting", "BeingMade"]
initial = "Setup"

[[state]]
name = "expected"
type = "string"
initial = "Setup"

[[state]]
name = "topping"
type = "string"
initial = "one"

[[state]]
name = "allowed"
type = "list"
initial = []

[[action]]
name = "Write"
from = ["Setup"]
to = "Waiting"
params = [{ name = "expected", type = "string" }]
effect = ["append(allowed, 'Secret')"]

[[action]]
name = "Enter"
from = ["Waiting"]
to = "BeingMade"
guard = "(expected == 'Setup' && Topping[topping].status == 'Stocked') || (expected in allowed && Topping[topping].status == 'Empty')"

[[entry_rule]]
name = "entry"
place = "BeingMade"
assert = "Topping[topping].status == 'Stocked'"
"#;

#[test]
fn effect_literal_membership_reports_the_breaking_two_move_path() {
    let model = build_model_from_ioa(SOURCE, 2).unwrap();
    let result = check_model(&model);
    assert!(result.is_complete, "{result:?}");
    let found = result
        .counterexamples
        .iter()
        .find(|c| c.property == "EntryRules")
        .unwrap_or_else(|| panic!("missing effect-literal entry: {result:?}"));
    assert_eq!(found.violated, ["entry"]);
    let [(before, Some(write)), (waiting, Some(enter)), (after, None)] = found.trace.as_slice()
    else {
        panic!("expected Write then Enter: {:?}", found.trace);
    };
    assert_eq!(before.status, "Setup");
    assert_eq!(write.name, "Write");
    assert_eq!(write.params["expected"], ParamValue::Str("Secret".into()));
    assert_eq!(waiting.status, "Waiting");
    assert_eq!(waiting.strings["expected"], ModelString::from("Secret"));
    assert_eq!(waiting.lists["allowed"], vec![ModelString::from("Secret")]);
    assert_eq!(enter.name, "Enter");
    assert_eq!(after.status, "BeingMade");
}

#[test]
fn effect_literal_membership_is_checked_by_simulation() {
    let model = build_model_from_ioa(SOURCE, 2).unwrap();
    let simulated = run_multi_seed_simulation(&model, &SimConfig::default(), 20);
    assert!(
        simulated
            .iter()
            .any(|s| s.violations.iter().any(|v| v.invariant == "entry")),
        "{simulated:?}"
    );
}

#[test]
fn effect_literal_membership_is_checked_by_property_tests() {
    let model = build_model_from_ioa(SOURCE, 2).unwrap();
    let tested = run_prop_tests_on_model(&model, 1000, 10);
    assert_eq!(
        tested.failure.as_ref().map(|f| f.invariant.as_str()),
        Some("entry"),
        "{tested:?}"
    );
}

#[test]
fn effect_literal_membership_with_a_covering_guard_passes() {
    let source = SOURCE.replace("== 'Empty'", "== 'Stocked'");
    let model = build_model_from_ioa(&source, 2).unwrap();
    let result = check_model(&model);
    assert!(result.is_complete, "{result:?}");
    assert!(
        !result
            .counterexamples
            .iter()
            .any(|c| c.property == "EntryRules"),
        "{result:?}"
    );
}

#[test]
fn assigned_effect_literal_exposes_the_breaking_input() {
    let source = SOURCE
        .replace(
            "name = \"allowed\"\ntype = \"list\"\ninitial = []",
            "name = \"allowed\"\ntype = \"string\"\ninitial = ''",
        )
        .replace("append(allowed, 'Secret')", "allowed = 'Secret'")
        .replace("expected in allowed", "expected == allowed");
    let model = build_model_from_ioa(&source, 2).unwrap();
    let result = check_model(&model);
    assert!(result.is_complete, "{result:?}");
    let found = result
        .counterexamples
        .iter()
        .find(|c| c.property == "EntryRules")
        .unwrap_or_else(|| panic!("missing assignment-literal entry: {result:?}"));
    let [(before, Some(write)), (waiting, Some(enter)), (after, None)] = found.trace.as_slice()
    else {
        panic!("expected Write then Enter: {:?}", found.trace);
    };
    assert_eq!(before.status, "Setup");
    assert_eq!(write.name, "Write");
    assert_eq!(write.params["expected"], ParamValue::Str("Secret".into()));
    assert_eq!(waiting.strings["allowed"], ModelString::from("Secret"));
    assert_eq!(enter.name, "Enter");
    assert_eq!(after.status, "BeingMade");
    let simulated = run_multi_seed_simulation(&model, &SimConfig::default(), 20);
    assert!(
        simulated
            .iter()
            .any(|s| s.violations.iter().any(|v| v.invariant == "entry"))
    );
    let tested = run_prop_tests_on_model(&model, 1000, 10);
    assert_eq!(
        tested.failure.as_ref().map(|f| f.invariant.as_str()),
        Some("entry")
    );
    let safe = build_model_from_ioa(&source.replace("== 'Empty'", "== 'Stocked'"), 2).unwrap();
    let checked = check_model(&safe);
    assert!(
        checked.is_complete && checked.all_properties_hold,
        "{checked:?}"
    );
}
