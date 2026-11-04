//! A pass that checked nothing is not a pass, and a module's answer nothing
//! names is walked as a move it may make: a rule whose premise no walked
//! state meets checks nothing, and a verdict resting on such an answer is
//! not a verdict.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

/// A record that asks a module when it starts. Its answer `Done` is declared
/// `done_kind`; with `on_success` the trigger names it, and without one the
/// module's result names the answer, as the kernel takes it. `Gone` is a
/// status nothing leads to. `rule` is appended as written.
fn ask(done_kind: &str, on_success: bool, rule: &str) -> String {
    let named = if on_success {
        "on_success = \"Done\"\n"
    } else {
        ""
    };
    format!(
        r#"
[automaton]
name = "Ask"
states = ["Idle", "Asked", "Done", "Gone"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Asked"

[[action.triggers]]
name = "ask"
kind = "wasm"
module = "asker"
{named}
[[action]]
name = "Done"
kind = "{done_kind}"
from = ["Asked"]
to = "Done"
{rule}
"#
    )
}

/// A box nothing fills.
const BOX: &str = r#"
[automaton]
name = "Box"
states = ["Empty", "Full"]
initial = "Empty"

[[action]]
name = "Fill"
kind = "internal"
from = ["Empty"]
to = "Full"
"#;

/// Only `Start` may be sent from outside; a module's answer skips Cedar.
const POLICIES: &str = r#"permit(principal, action == Action::"Start", resource);"#;

fn parsed(sources: &[&str]) -> Vec<Automaton> {
    sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect()
}

/// Run `check` in a room no fixture here comes near filling.
fn bounded<T>(check: impl FnOnce(Bound<'_>) -> T) -> T {
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    check(Bound {
        room: &room,
        progress: &progress,
    })
}

fn liveness(done_kind: &str, on_success: bool, eventually: &str) -> JointLivenessResult {
    let rule = format!("[[liveness]]\nname = \"rule\"\neventually = \"{eventually}\"\n");
    let automatons = parsed(&[&ask(done_kind, on_success, &rule)]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(POLICIES).expect("fixture policies parse");
    let mut results = bounded(|bound| verify_joint_liveness(&refs, Some(&analysis), bound));
    assert_eq!(results.len(), 1);
    results.remove(0)
}

fn invariant(done_kind: &str, assert: &str) -> JointInvariantResult {
    let rule = format!("[[invariant]]\nname = \"rule\"\nassert = \"{assert}\"\n");
    let automatons = parsed(&[&ask(done_kind, false, &rule), BOX]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(POLICIES).expect("fixture policies parse");
    let mut results = bounded(|bound| verify_joint_invariants(&refs, Some(&analysis), bound));
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn a_stuck_state_only_an_unnamed_module_answer_reaches_is_not_checked() {
    // The module may answer Done, and nothing then leads to Gone. Whether it
    // ever does is not known, so this is not a broken rule.
    let result = liveness("input", false, "status == 'Done' => status == 'Gone'");
    let text = result.to_string();
    assert!(!result.holds(), "{text}");
    assert!(text.contains("NOT CHECKED"), "{text}");
    assert!(text.contains("answer from asker"), "{text}");
}

#[test]
fn a_liveness_premise_no_walked_state_meets_proves_nothing() {
    let result = liveness("internal", false, "status == 'Gone' => status == 'Done'");
    let text = result.to_string();
    assert!(!result.holds(), "{text}");
    assert!(text.contains("CHECKS NOTHING AS WRITTEN"), "{text}");
    assert!(text.contains("`status == 'Gone'`"), "{text}");
}

#[test]
fn a_break_only_an_unnamed_module_answer_reaches_is_not_checked() {
    // If the module answers Done, the box is still Empty, which breaks the
    // rule. Whether it ever does is not known.
    let result = invariant("input", "status == 'Done' => Box[Id].status == 'Full'");
    let text = result.to_string();
    assert!(!result.holds(), "{text}");
    assert!(text.contains("NOT CHECKED"), "{text}");
    assert!(text.contains("answer from asker"), "{text}");
}

#[test]
fn an_invariant_premise_no_settled_state_meets_proves_nothing() {
    let result = invariant("internal", "status == 'Gone' => Box[Id].status == 'Full'");
    let text = result.to_string();
    assert!(!result.holds(), "{text}");
    assert!(text.contains("CHECKS NOTHING AS WRITTEN"), "{text}");
}

#[test]
fn a_failed_call_nothing_retries_is_stuck_whatever_the_module_may_answer() {
    // The module's call can fail, and then nothing leads on: the answer it
    // might have given does not change that.
    let result = liveness("input", false, "status == 'Asked' => status == 'Done'");
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{text}"
    );
}

#[test]
fn an_unnamed_answer_that_is_the_only_way_out_can_be_no_answer() {
    // A failed call asks again, and only the module's answer leads on. The
    // trigger names no answer, so the call can also end with none, and with
    // no timer the record stays where it is.
    let spec = ask(
        "input",
        false,
        r#"
[[action]]
name = "Again"
kind = "internal"
from = ["Asked"]
to = "Asked"

[[action.triggers]]
name = "ask_again"
kind = "wasm"
module = "asker"
on_failure = "Again"

[[liveness]]
name = "rule"
eventually = "status == 'Asked' => status == 'Done'"
"#,
    )
    .replace(
        "module = \"asker\"\n\n[[action]]\nname = \"Done\"",
        "module = \"asker\"\non_failure = \"Again\"\n\n[[action]]\nname = \"Done\"",
    );
    let automatons = parsed(&[&spec]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(POLICIES).expect("fixture policies parse");
    let mut results = bounded(|bound| verify_joint_liveness(&refs, Some(&analysis), bound));
    let result = results.remove(0);
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{result}"
    );
}

#[test]
fn a_rule_that_holds_through_an_unnamed_answer_holds() {
    // The walk goes where the answer leads, and the rule holds there.
    let result = liveness("input", false, "status == 'Done' => status == 'Done'");
    assert!(result.holds(), "{result}");
}

#[test]
fn a_premise_the_walk_meets_settles() {
    let result = liveness("internal", false, "status == 'Done' => status == 'Done'");
    assert!(result.holds(), "{result}");
}

#[test]
fn an_answer_the_trigger_names_is_walked() {
    // Done is declared input and no one outside may send it, but the
    // trigger names it, so the model takes it as the module's answer.
    let result = liveness("input", true, "status == 'Done' => status == 'Done'");
    assert!(result.holds(), "{result}");
}

#[test]
fn an_invariant_the_walk_breaks_stays_broken() {
    let result = invariant("internal", "status == 'Done' => Box[Id].status == 'Full'");
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}
