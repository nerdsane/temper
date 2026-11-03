//! A counter nothing reads is left out of the model, as a string nothing
//! reads already is, so it neither bounds a pass nor multiplies the values a
//! move tries.

use stateright::Model;
use temper_verify::model::{build_model_from_ioa, semantics::successors};
use temper_verify::{CascadeLevel, VerificationCascade};

/// A draft that every move adds to `version`, which no rule reads, and that
/// publishes once `approvals`, which the rule reads, is above 0.
const VERSIONED: &str = r#"
[automaton]
name = "Versioned"
states = ["Draft", "Published"]
initial = "Draft"

[[state]]
name = "version"
type = "counter"
initial = 0

[[state]]
name = "approvals"
type = "counter"
initial = 0

[[action]]
name = "Edit"
from = ["Draft"]
effect = ["version += 1"]

[[action]]
name = "Approve"
from = ["Draft"]
guard = "approvals < 1"
effect = ["approvals += 1", "version += 1"]

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
guard = "approvals > 0"
effect = ["version += 1"]

[[action]]
name = "Log"
from = ["Draft"]
effect = ["version += params.lines"]

[[invariant]]
name = "Approved"
assert = "status == 'Published' => approvals > 0"
"#;

#[test]
fn a_counter_nothing_reads_does_not_bound_a_pass() {
    let result = VerificationCascade::from_ioa(VERSIONED)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();
    assert!(result.all_passed, "{result:?}");
    let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
    assert!(l1.summary.contains("all properties hold"), "{}", l1.summary);
    let cut = &l1.verification.as_ref().unwrap().bounded_by;
    assert!(cut.is_empty(), "{cut:?}");
}

/// `Log` adds `params.lines` only to `version`, so any number of lines ends
/// the move the same way, and one is tried.
#[test]
fn a_parameter_only_an_unread_counter_takes_is_tried_once() {
    let model = build_model_from_ioa(VERSIONED, 2).unwrap();
    let initial = model.init_states().remove(0);
    let log = model.transitions.iter().find(|t| t.name == "Log").unwrap();
    let choices = successors(&model, log, &initial);
    assert_eq!(choices.len(), 1, "{choices:?}");
}

/// `shown = count` makes `count` read, through `shown`, which the rule
/// reads: the rule breaks once `count` reaches 2.
#[test]
fn a_counter_copied_into_a_read_one_is_kept() {
    let spec = r#"
[automaton]
name = "Shown"
states = ["On"]
initial = "On"

[[state]]
name = "count"
type = "counter"
initial = 0

[[state]]
name = "shown"
type = "counter"
initial = 0

[[action]]
name = "Tick"
from = ["On"]
effect = ["count += 1"]

[[action]]
name = "Show"
from = ["On"]
effect = ["shown = count"]

[[invariant]]
name = "Small"
assert = "shown < 2"
"#;
    let result = VerificationCascade::from_ioa(spec)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();
    let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
    assert!(!l1.passed, "{}", l1.summary);
    let found = l1
        .verification
        .as_ref()
        .unwrap()
        .counterexamples
        .iter()
        .find(|c| c.violated.iter().any(|name| name == "Small"))
        .unwrap_or_else(|| panic!("no counterexample for Small: {}", l1.summary));
    let (last, _) = found.trace.last().unwrap();
    assert_eq!(last.counters["shown"], 2);
}

/// An entry rule reads `count` even though no guard or invariant does.
/// Entering after Tick must still produce the path that breaks the rule.
#[test]
fn a_counter_read_only_by_an_entry_rule_is_kept() {
    let spec = r#"
[automaton]
name = "EntryCount"
states = ["Outside", "Inside"]
initial = "Outside"
terminal = ["Inside"]

[[state]]
name = "count"
type = "counter"
initial = 0

[[action]]
name = "Tick"
from = ["Outside"]
effect = ["count += 1"]

[[action]]
name = "Enter"
from = ["Outside"]
to = "Inside"

[[entry_rule]]
name = "enter_before_counting"
place = "Inside"
assert = "count == 0"
"#;
    let result = VerificationCascade::from_ioa(spec)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();
    assert!(!result.all_passed, "{result:?}");
    let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
    assert!(!l1.passed, "{}", l1.summary);
    let found = l1
        .verification
        .as_ref()
        .unwrap()
        .counterexamples
        .iter()
        .find(|c| c.property == "EntryRules" && c.violated == ["enter_before_counting"])
        .unwrap_or_else(|| panic!("no path breaking the entry rule: {}", l1.summary));
    let [.., (before, Some(action)), (after, None)] = found.trace.as_slice() else {
        panic!("the path ends without its move: {:?}", found.trace);
    };
    assert_eq!(before.status, "Outside");
    assert_eq!(before.counters["count"], 1);
    assert_eq!(action.name, "Enter");
    assert_eq!(after.status, "Inside");
}
