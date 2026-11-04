//! A module call can end by answering nothing, and the record stays where it
//! is. A state only such a call leads out of has no sure way out.

use super::*;

/// A job that `Go` makes busy with a module call, which fails to `Give`.
/// `answers` is what its trigger says it answers, and `timer` is appended.
fn job(answers: &str, timer: &str) -> String {
    format!(
        r#"
[automaton]
name = "Job"
states = ["Idle", "Busy", "Done"]
initial = "Idle"

[[action]]
name = "Go"
kind = "input"
from = ["Idle"]
to = "Busy"

[[action.triggers]]
name = "work"
kind = "wasm"
module = "steps"
{answers}
on_failure = "Give"

[[action]]
name = "Finish"
kind = "internal"
from = ["Busy"]
to = "Done"

[[action]]
name = "Give"
kind = "internal"
from = ["Busy"]
to = "Done"

{timer}

[[liveness]]
name = "ends"
eventually = "status != 'Busy'"
"#
    )
}

fn check_job(answers: &str, timer: &str) -> JointLivenessResult {
    let automaton = parse_automaton(&job(answers, timer)).expect("fixture parses");
    let analysis = SendAnalysis::new(r#"permit(principal, action == Action::"Go", resource);"#)
        .expect("policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&[&automaton], Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn a_call_that_answers_nothing_leaves_a_state_with_no_timer_stuck() {
    let result = check_job("", "");
    let LivenessOutcome::Stuck { cases, .. } = &result.outcome else {
        panic!("the busy state has no way out once its call ends quietly: {result}");
    };
    assert!(
        cases.iter().any(|case| {
            case.path.iter().any(|step| {
                step.action
                    .as_deref()
                    .is_some_and(|action| action.ends_with("ends with no answer"))
            })
        }),
        "{result}"
    );
}

#[test]
fn a_timer_is_a_way_out_when_the_call_answers_nothing() {
    let result = check_job(
        "",
        "[[state_timeout]]\nstate = \"Busy\"\nafter_seconds = 60\non_timeout = \"Give\"",
    );
    assert!(result.holds(), "{result}");
}

#[test]
fn a_trigger_that_names_its_answers_can_still_answer_nothing() {
    let result = check_job("on_success = \"Finish\"", "");
    let LivenessOutcome::Stuck { cases, .. } = &result.outcome else {
        panic!("naming an answer does not promise one comes: {result}");
    };
    assert!(
        cases.iter().any(|case| {
            case.path.iter().any(|step| {
                step.action
                    .as_deref()
                    .is_some_and(|action| action.ends_with("ends with no answer"))
            })
        }),
        "{result}"
    );
}

#[test]
fn a_timer_is_a_way_out_of_a_trigger_that_names_its_answers() {
    let result = check_job(
        "on_success = \"Finish\"",
        "[[state_timeout]]\nstate = \"Busy\"\nafter_seconds = 60\non_timeout = \"Give\"",
    );
    assert!(result.holds(), "{result}");
}

#[test]
fn a_named_answer_that_does_not_fit_the_record_ends_with_none() {
    // `Finish` is taken only from `Done`, so the record never takes it while
    // busy, and the kernel makes no move for the call.
    let automaton = parse_automaton(&job("on_success = \"Finish\"", "").replace(
        "name = \"Finish\"\nkind = \"internal\"\nfrom = [\"Busy\"]",
        "name = \"Finish\"\nkind = \"internal\"\nfrom = [\"Done\"]",
    ))
    .expect("fixture parses");
    let analysis = SendAnalysis::new(r#"permit(principal, action == Action::"Go", resource);"#)
        .expect("policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let result = verify_joint_liveness(&[&automaton], Some(&analysis), bound).remove(0);
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{result}"
    );
}
