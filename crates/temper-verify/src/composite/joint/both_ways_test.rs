//! A request Cedar's answer to which depends on who asks and a reaction from a
//! type outside a rule's scope are each walked as moves that can happen. A break
//! through one is a break, and its verdict names what it rests on. A way that
//! says nothing stays not checked: an answer on the record's fields alone, one
//! needing Cedar to answer one request two ways, or a reaction from outside
//! carrying values the walk made up.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

/// Reactions get through: only requests are left for each test to decide.
const REACTIONS: &str =
    r#"permit(principal, action, resource) when { context has via && context.via != "request" };"#;

/// A job with no way out of `Busy`. `start` is added to its `Start`, and its
/// `note`, which no guard or rule reads, is not held by the walk.
fn job(start: &str) -> String {
    format!(
        r#"
[automaton]
name = "Job"
states = ["Idle", "Busy"]
initial = "Idle"

[[state]]
name = "note"
type = "string"
initial = "fine"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Busy"
{start}

[[liveness]]
name = "ends"
eventually = "status != 'Busy'"
"#
    )
}

/// A type outside the job's rule, whose `Tend` starts the job. `args` is what
/// its trigger sends.
fn keeper(args: &str) -> String {
    format!(
        r#"
[automaton]
name = "Keeper"
states = ["Open"]
initial = "Open"

[[action]]
name = "Tend"
kind = "input"
from = ["Open"]

[[action.triggers]]
name = "start"
kind = "entity"
target_entity = "Job"
target_action = "Start"
{args}

[action.triggers.resolve_target]
kind = "same_id"
"#
    )
}

fn bounded<T>(check: impl FnOnce(Bound<'_>) -> T) -> T {
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    check(Bound {
        room: &room,
        progress: &progress,
    })
}

fn parsed(sources: &[&str]) -> Vec<Automaton> {
    sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect()
}

fn liveness(sources: &[&str], policies: &str) -> JointLivenessResult {
    let automatons = parsed(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let mut results = bounded(|bound| verify_joint_liveness(&refs, Some(&analysis), bound));
    assert_eq!(results.len(), 1);
    results.remove(0)
}

fn invariant(sources: &[&str], policies: &str) -> JointInvariantResult {
    let automatons = parsed(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let mut results = bounded(|bound| verify_joint_invariants(&refs, Some(&analysis), bound));
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn a_state_with_no_way_out_on_a_sure_request_never_settles_with_no_condition() {
    let policies = r#"permit(principal, action == Action::"Start", resource);"#;
    let result = liveness(&[&job("")], policies);
    let text = result.to_string();
    assert!(text.contains("NEVER SETTLES:"), "{text}");
}

#[test]
fn a_state_with_no_way_out_on_an_undecided_request_never_settles_if_cedar_permits_it() {
    let policies = r#"permit(principal, action == Action::"Start", resource is Job) when {
    principal has id && principal.id == resource.note
};"#;
    let result = liveness(&[&job("")], policies);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{text}"
    );
    assert!(
        text.contains("NEVER SETTLES if the runtime can make"),
        "{text}"
    );
    assert!(text.contains("Job.Start"), "{text}");
}

#[test]
fn an_answer_on_the_record_alone_says_nothing() {
    // One answer for every caller, and it may never be `permit`.
    let policies = r#"permit(principal, action == Action::"Start", resource is Job) when {
    resource.note == "fine"
};"#;
    let result = liveness(&[&job("")], policies);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Incomplete { .. }),
        "{text}"
    );
}

#[test]
fn a_state_only_a_reaction_from_outside_reaches_never_settles_if_it_comes() {
    let result = liveness(&[&job(""), &keeper("")], REACTIONS);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{text}"
    );
    assert!(
        text.contains("NEVER SETTLES if the runtime can make"),
        "{text}"
    );
    assert!(text.contains("a reaction from Keeper"), "{text}");
}

#[test]
fn a_reaction_from_outside_with_made_up_values_says_nothing() {
    let start = r#"params = [{ name = "note", type = "string" }]"#;
    let args = "\n[action.triggers.args]\nnote = \"'fine'\"\n";
    // The rule reads what it carries, so the value is part of the walk.
    let job = job(start).replace("status != 'Busy'", "status != 'Busy' || note == 'never'");
    let result = liveness(&[&job, &keeper(args)], REACTIONS);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Incomplete { .. }),
        "{text}"
    );
    assert!(text.contains("a reaction from Keeper"), "{text}");
}

/// A parent whose two moves each ask to note its child.
const PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["P0", "P1", "P2"]
initial = "P0"

[[action]]
name = "First"
kind = "input"
from = ["P0"]
to = "P1"

[[action.triggers]]
name = "note"
kind = "entity"
target_entity = "Child"
target_action = "Note"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Second"
kind = "input"
from = ["P1"]
to = "P2"

[[action.triggers]]
name = "note"
kind = "entity"
target_entity = "Child"
target_action = "Note"

[action.triggers.resolve_target]
kind = "same_id"
"#;

/// A child counting its notes, with a rule on the count once the parent is
/// done. Cedar's answer to `Note` turns on who asks.
fn child(notes: u32) -> String {
    format!(
        r#"
[automaton]
name = "Child"
states = ["Open"]
initial = "Open"

[[state]]
name = "notes"
type = "counter"
initial = 0

[[state]]
name = "tag"
type = "string"
initial = "x"

[[action]]
name = "Note"
kind = "internal"
from = ["Open"]
effect = ["notes += 1"]

[[invariant]]
name = "rule"
assert = "Parent[Id].status != 'P2' || notes != {notes}"
"#
    )
}

const NOTE_ON_WHO_ASKS: &str = r#"
permit(principal, action, resource is Parent);
permit(principal, action == Action::"Note", resource is Child) when {
    context has via && context.via != "request" && principal has id && principal.id == resource.tag
};
"#;

#[test]
fn a_break_needing_cedar_to_answer_one_request_two_ways_stays_not_checked() {
    // One note out of two asks needs one request both permitted and refused.
    let result = invariant(&[PARENT, &child(1)], NOTE_ON_WHO_ASKS);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, JointOutcome::Incomplete { .. }),
        "{text}"
    );
}

#[test]
fn a_break_needing_cedar_to_answer_one_way_is_broken_if_it_does() {
    let result = invariant(&[PARENT, &child(2)], NOTE_ON_WHO_ASKS);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{text}"
    );
    assert!(text.contains("BROKEN if the runtime can make"), "{text}");
    assert!(text.contains("Child.Note"), "{text}");
}

#[test]
fn a_break_where_cedar_refuses_every_time_is_broken_if_it_does() {
    let result = invariant(&[PARENT, &child(0)], NOTE_ON_WHO_ASKS);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{text}"
    );
    assert!(text.contains("BROKEN if the runtime can make"), "{text}");
    assert!(text.contains("refused by Cedar"), "{text}");
}

#[test]
fn a_request_with_a_guard_on_its_own_field_never_settles_if_it_is_met() {
    // `Mark` sets the field to whatever it is sent, so the walk cannot decide
    // `Start`'s guard: a sender can meet it or not.
    let start = "guard = \"note != 'held'\"\n";
    let job = job(start).replacen(
        "[[action]]\nname = \"Start\"",
        "[[action]]\nname = \"Mark\"\nkind = \"input\"\nfrom = [\"Idle\"]\nparams = [{ name = \"note\", type = \"string\" }]\n\n[[action]]\nname = \"Start\"",
        1,
    );
    let policies = r#"permit(principal, action, resource);"#;
    let result = liveness(&[&job], policies);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{text}"
    );
}
