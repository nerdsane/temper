//! A pass that checked nothing is not a pass: a rule whose premise no walked
//! state meets, and a walk that never takes an answer a module may give.

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
fn a_liveness_pass_that_never_takes_a_module_answer_is_not_checked() {
    // The module may answer Done, and nothing then leads to Gone: the
    // runtime gets stuck. The walk never takes Done, so it never meets the
    // premise.
    let result = liveness("input", false, "status == 'Done' => status == 'Gone'");
    let text = result.to_string();
    assert!(!result.holds(), "{text}");
    assert!(text.contains("NOT CHECKED"), "{text}");
    assert!(text.contains("Ask.Done (from Asked)"), "{text}");
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
fn an_invariant_that_holds_without_a_module_answer_is_not_checked() {
    // Once the module answers Done, the box is still Empty: the runtime
    // breaks the rule. The walk never takes Done.
    let result = invariant("input", "status == 'Done' => Box[Id].status == 'Full'");
    let text = result.to_string();
    assert!(!result.holds(), "{text}");
    assert!(text.contains("NOT CHECKED"), "{text}");
    assert!(text.contains("Ask.Done (from Asked)"), "{text}");
}

#[test]
fn an_invariant_premise_no_settled_state_meets_proves_nothing() {
    let result = invariant("internal", "status == 'Gone' => Box[Id].status == 'Full'");
    let text = result.to_string();
    assert!(!result.holds(), "{text}");
    assert!(text.contains("CHECKS NOTHING AS WRITTEN"), "{text}");
}

#[test]
fn a_stuck_verdict_names_the_answers_the_walk_never_takes() {
    let result = liveness("input", false, "status == 'Asked' => status == 'Done'");
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{text}"
    );
    assert!(text.contains("Ask.Done (from Asked)"), "{text}");
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
