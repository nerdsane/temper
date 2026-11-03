//! A commit's reactions in the runtime's order: one trigger at a time, each
//! reaction and its cascade before the next trigger's guard is read.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

/// Without `policies`, anyone may send anything.
fn invariant(sources: &[&str], policies: Option<&str>) -> JointInvariantResult {
    let automatons: Vec<Automaton> = sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = policies.map(|p| SendAnalysis::new(p).expect("fixture policies parse"));
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&refs, analysis.as_ref(), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

/// `Go` shuts `B`, then notes `A` once `B` is shut. A gone record is never
/// left with its box shut and not noted: the runtime reads `note`'s guard
/// only after `shut` has landed.
const GO: &str = r#"
[automaton]
name = "A"
states = ["Idle", "Went", "Noted"]
initial = "Idle"

[[action]]
name = "Go"
kind = "input"
from = ["Idle"]
to = "Went"

[[action.triggers]]
name = "shut"
kind = "entity"
target_entity = "B"
target_action = "Shut"

[action.triggers.resolve_target]
kind = "same_id"

[[action.triggers]]
name = "note"
kind = "entity"
target_entity = "A"
target_action = "Note"
guard = "B[Id].status == 'Shut'"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Note"
kind = "internal"
from = ["Went"]
to = "Noted"

[[invariant]]
name = "rule"
assert = "status == 'Went' => B[Id].status == 'Open'"
"#;

const SHUT: &str = r#"
[automaton]
name = "B"
states = ["Open", "Shut"]
initial = "Open"

[[action]]
name = "Shut"
kind = "internal"
from = ["Open"]
to = "Shut"
"#;

#[test]
fn a_later_trigger_guard_is_not_read_before_an_earlier_reaction_lands() {
    // Read before shut lands, note's guard is false and A stays Went with B
    // shut. The runtime never does that, so the walk cannot be sure of it.
    let policies = r#"permit(principal, action == Action::"Go", resource);
permit(principal, action, resource) when { context has via && context.via != "request" };"#;
    let result = invariant(&[GO, SHUT], Some(policies));
    assert!(
        !matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

/// `Go` finishes `A` and has four triggers whose guards read `B`, which is
/// never in the state they ask for: the runtime sends none of them.
const FOUR: &str = r#"
[automaton]
name = "A"
states = ["Start", "Done"]
initial = "Start"

[[action]]
name = "Go"
kind = "input"
from = ["Start"]
to = "Done"

[[action.triggers]]
name = "one"
kind = "entity"
target_entity = "A"
target_action = "Ping"
guard = "B[Id].status == 'Never'"
[action.triggers.resolve_target]
kind = "same_id"

[[action.triggers]]
name = "two"
kind = "entity"
target_entity = "A"
target_action = "Ping"
guard = "B[Id].status == 'Never'"
[action.triggers.resolve_target]
kind = "same_id"

[[action.triggers]]
name = "three"
kind = "entity"
target_entity = "A"
target_action = "Ping"
guard = "B[Id].status == 'Never'"
[action.triggers.resolve_target]
kind = "same_id"

[[action.triggers]]
name = "four"
kind = "entity"
target_entity = "A"
target_action = "Ping"
guard = "B[Id].status == 'Never'"
[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Ping"
kind = "internal"
from = ["Done"]
to = "Done"

[[invariant]]
name = "rule"
assert = "status != 'Done' || B[Id].status == 'Never'"
"#;

const READY: &str = r#"
[automaton]
name = "B"
states = ["Ready", "Never"]
initial = "Ready"

[[action]]
name = "Wait"
kind = "internal"
from = ["Never"]
to = "Never"
"#;

#[test]
fn guards_not_yet_read_do_not_count_as_reactions_in_flight() {
    // Go finishes A and sends nothing: the rule breaks after one move.
    let policies = r#"permit(principal, action == Action::"Go", resource);
permit(principal, action, resource) when { context has via && context.via != "request" };"#;
    let result = invariant(&[FOUR, READY], Some(policies));
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

/// `Go` notes `A` once `B` is shut. `Shut`, allowed only once `A` has gone,
/// pings `B` only while `A` is idle, which it no longer is. For `A` to be
/// noted, `Shut` commits while `note`'s guard is unread, so both commits
/// wait on one guard at once.
const NOTE: &str = r#"
[automaton]
name = "A"
states = ["Idle", "Went", "Noted"]
initial = "Idle"

[[action]]
name = "Go"
kind = "input"
from = ["Idle"]
to = "Went"

[[action.triggers]]
name = "note"
kind = "entity"
target_entity = "A"
target_action = "Note"
guard = "B[Id].status == 'Shut'"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Note"
kind = "internal"
from = ["Went"]
to = "Noted"

[[invariant]]
name = "rule"
assert = "status == 'Noted' => B[Id].status == 'Open'"
"#;

const PING: &str = r#"
[automaton]
name = "B"
states = ["Open", "Shut"]
initial = "Open"

[[action]]
name = "Shut"
kind = "input"
from = ["Open"]
to = "Shut"
guard = "A[Id].status == 'Went'"

[[action.triggers]]
name = "ping"
kind = "entity"
target_entity = "B"
target_action = "Ping"
guard = "A[Id].status == 'Idle'"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Ping"
kind = "internal"
from = ["Shut"]
to = "Shut"
"#;

#[test]
fn unread_guards_of_two_commits_fit_the_in_flight_bound() {
    // Go, then Shut, then both guards read: A is noted with B shut. Two
    // commits each waiting on one guard are within the bound the walk keeps.
    let policies = r#"permit(principal, action in [Action::"Go", Action::"Shut"], resource);
permit(principal, action in [Action::"Note", Action::"Ping"], resource) when { context.via == "reaction" };"#;
    let result = invariant(&[NOTE, PING], Some(policies));
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}
