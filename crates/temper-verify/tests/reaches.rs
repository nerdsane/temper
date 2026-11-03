//! A `[[liveness]]` rule with `reaches` says every record in one of its
//! `from` places gets to one of the places it names, whatever moves it
//! makes: no path from there may stop short of them or go round forever
//! without them. The spec language has no fairness, so a loop any record
//! can keep taking breaks the rule. Without `from`, the rule is read from a
//! new record.
//!
//! Each rule is its own: one rule's places never count for another.

use temper_verify::{CascadeLevel, LevelResult, VerificationCascade};

/// A draft goes to review, and review sends it back or approves it.
const SENT_BACK: &str = r#"
[automaton]
name = "Loop"
states = ["Draft", "Review", "Done"]
initial = "Draft"

[[action]]
name = "Submit"
from = ["Draft"]
to = "Review"

[[action]]
name = "SendBack"
from = ["Review"]
to = "Draft"

[[action]]
name = "Approve"
from = ["Review"]
to = "Done"

[[liveness]]
name = "ReviewEnds"
from = ["Review"]
reaches = ["Done"]
"#;

/// An open record can be pinged, staying open, or closed.
const PINGED: &str = r#"
[automaton]
name = "SelfLoop"
states = ["Open", "Closed"]
initial = "Open"

[[action]]
name = "Ping"
from = ["Open"]
to = "Open"

[[action]]
name = "Close"
from = ["Open"]
to = "Closed"

[[liveness]]
name = "Closes"
from = ["Open"]
reaches = ["Closed"]
"#;

/// A draft is discarded or reviewed; review approves or rejects.
const TWO_RULES: &str = r#"
[automaton]
name = "Merged"
states = ["Draft", "Review", "Done", "Rejected", "Discarded"]
initial = "Draft"

[[action]]
name = "Submit"
from = ["Draft"]
to = "Review"

[[action]]
name = "Discard"
from = ["Draft"]
to = "Discarded"

[[action]]
name = "Approve"
from = ["Review"]
to = "Done"

[[action]]
name = "Reject"
from = ["Review"]
to = "Rejected"

[[liveness]]
name = "Settles"
from = ["Draft"]
reaches = ["Done", "Rejected", "Discarded"]

[[liveness]]
name = "ReviewApproves"
from = ["Review"]
reaches = ["Done"]
"#;

/// A draft is discarded or reviewed, and review always approves.
const REVIEW_APPROVES: &str = r#"
[automaton]
name = "FromHonoured"
states = ["Draft", "Review", "Done", "Discarded"]
initial = "Draft"

[[action]]
name = "Submit"
from = ["Draft"]
to = "Review"

[[action]]
name = "Discard"
from = ["Draft"]
to = "Discarded"

[[action]]
name = "Approve"
from = ["Review"]
to = "Done"

[[liveness]]
name = "ReviewApproves"
from = ["Review"]
reaches = ["Done"]
"#;

/// A new record is published, then archived; nothing goes back.
const PUBLISHED_THEN_ARCHIVED: &str = r#"
[automaton]
name = "NoFrom"
states = ["Draft", "Published", "Archived"]
initial = "Draft"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"

[[action]]
name = "Archive"
from = ["Published"]
to = "Archived"

[[liveness]]
name = "GetsPublished"
reaches = ["Published"]
"#;

/// The cascade as an install runs it, with its L1 result.
fn install_check(spec: &str) -> (bool, LevelResult) {
    let result = VerificationCascade::from_ioa(spec)
        .with_sim_seeds(3)
        .with_prop_test_cases(20)
        .run();
    let l1 = result
        .level_result(CascadeLevel::ModelCheck)
        .cloned()
        .expect("L1 ran");
    (result.all_passed, l1)
}

/// The places along the path L1 gives for the broken rule `name`, and
/// whether its last state is one the path already passed through.
fn broken(l1: &LevelResult, name: &str) -> (Vec<String>, bool) {
    assert!(!l1.passed, "{}", l1.summary);
    let verification = l1.verification.as_ref().expect("L1 ran a model check");
    let found = verification
        .counterexamples
        .iter()
        .find(|c| c.violated == [name])
        .unwrap_or_else(|| panic!("no path for {name}: {:?}", l1.diagnostics));
    let places = found.trace.iter().map(|(s, _)| s.status.clone()).collect();
    let (last, _) = found.trace.last().expect("a path has a state");
    let loops = found.trace[..found.trace.len() - 1]
        .iter()
        .any(|(s, _)| s == last);
    (places, loops)
}

#[test]
fn a_record_sent_back_forever_breaks_the_rule() {
    let (passed, l1) = install_check(SENT_BACK);
    assert!(!passed, "{}", l1.summary);
    let (places, loops) = broken(&l1, "ReviewEnds");
    assert!(loops, "the path goes round a loop: {places:?}");
    assert!(!places.contains(&"Done".to_string()), "{places:?}");
    assert!(places.contains(&"Review".to_string()), "{places:?}");
}

#[test]
fn a_move_that_stays_in_place_forever_breaks_the_rule() {
    let (passed, l1) = install_check(PINGED);
    assert!(!passed, "{}", l1.summary);
    let (places, loops) = broken(&l1, "Closes");
    assert!(loops, "the path goes round a loop: {places:?}");
    assert_eq!(places, ["Open", "Open"]);
}

#[test]
fn one_rules_places_do_not_count_for_another() {
    let (passed, l1) = install_check(TWO_RULES);
    assert!(!passed, "{}", l1.summary);
    let (places, loops) = broken(&l1, "ReviewApproves");
    assert!(!loops, "{places:?}");
    assert_eq!(places, ["Draft", "Review", "Rejected"]);
    let named: Vec<&Vec<String>> = l1
        .verification
        .as_ref()
        .unwrap()
        .counterexamples
        .iter()
        .map(|c| &c.violated)
        .collect();
    assert_eq!(
        named,
        [&vec!["ReviewApproves".to_string()]],
        "Settles holds"
    );
}

#[test]
fn a_rule_is_read_only_from_its_own_places() {
    let (passed, l1) = install_check(REVIEW_APPROVES);
    assert!(passed && l1.passed, "{}: {:?}", l1.summary, l1.diagnostics);
}

#[test]
fn a_rule_without_from_is_read_from_a_new_record() {
    let (_, l1) = install_check(PUBLISHED_THEN_ARCHIVED);
    assert!(l1.passed, "{}: {:?}", l1.summary, l1.diagnostics);
}

/// A simulated record that was published meets the rule, though its run
/// ends after it is archived.
#[test]
fn a_run_that_passed_through_the_target_meets_the_rule_wherever_it_ends() {
    let result = VerificationCascade::from_ioa(PUBLISHED_THEN_ARCHIVED)
        .with_sim_seeds(3)
        .with_prop_test_cases(20)
        .run();
    let l2 = result
        .level_result(CascadeLevel::Simulation)
        .expect("L2 ran");
    assert!(l2.passed, "{}: {:?}", l2.summary, l2.diagnostics);
    assert!(result.all_passed, "{:?}", result.levels);
}

/// A waiting record's only move needs `score == 1`, and nothing writes
/// `score`, so a record can wait forever. L1 does not track an integer, so
/// it takes the move as one that may fire: the walk cannot tell this record
/// from one that finishes.
const SCORE_GATE: &str = r#"
[automaton]
name = "NumGate"
states = ["Waiting", "Done"]
initial = "Waiting"

[[state]]
name = "score"
type = "int"
initial = 0

[[action]]
name = "Finish"
from = ["Waiting"]
to = "Done"
guard = "score == 1"

[[liveness]]
name = "Finishes"
from = ["Waiting"]
reaches = ["Done"]
"#;

#[test]
fn a_rule_that_needs_a_guard_the_walk_does_not_track_is_not_proved() {
    let (passed, l1) = install_check(SCORE_GATE);
    assert!(!passed && !l1.passed, "{}", l1.summary);
    assert!(l1.summary.contains("not proved"), "{}", l1.summary);
    assert!(l1.summary.contains("score"), "{}", l1.summary);
    let verification = l1.verification.as_ref().unwrap();
    assert!(verification.is_complete, "the walk completed");
    assert!(
        verification.counterexamples.is_empty(),
        "no path is known to break it"
    );
}

/// A draft is published once its file is ready: a move gated on another
/// record, which may never open.
const FILE_GATE: &str = r#"
[automaton]
name = "Gated"
states = ["Draft", "Published"]
initial = "Draft"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
guard = "File[file_id].status in ['Ready']"

[[liveness]]
name = "GetsPublished"
from = ["Draft"]
reaches = ["Published"]
"#;

#[test]
fn a_rule_that_rests_on_a_gate_another_record_holds_is_not_proved() {
    let (passed, l1) = install_check(FILE_GATE);
    assert!(!passed && !l1.passed, "{}", l1.summary);
    assert!(l1.summary.contains("not proved"), "{}", l1.summary);
    assert!(l1.summary.contains("GetsPublished"), "{}", l1.summary);
    let verification = l1.verification.as_ref().unwrap();
    assert!(verification.is_complete, "the walk completed");
    assert!(verification.counterexamples.is_empty());
}
