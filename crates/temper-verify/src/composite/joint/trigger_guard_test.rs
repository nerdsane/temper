//! A trigger's guard that reads a related record: the runtime reads it after
//! its source commits, one trigger after another, while other moves land.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

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

/// Without `policies`, anyone may send anything.
fn invariant(sources: &[&str], policies: Option<&str>) -> JointInvariantResult {
    let automatons = parsed(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = policies.map(|p| SendAnalysis::new(p).expect("fixture policies parse"));
    let mut results = bounded(|bound| verify_joint_invariants(&refs, analysis.as_ref(), bound));
    assert_eq!(results.len(), 1);
    results.remove(0)
}

fn liveness(sources: &[&str]) -> JointLivenessResult {
    let automatons = parsed(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let mut results = bounded(|bound| verify_joint_liveness(&refs, None, bound));
    assert_eq!(results.len(), 1);
    results.remove(0)
}

/// A post that opens its thread when the thread it names is not made yet.
/// `rule` is appended as written.
fn post(rule: &str) -> String {
    format!(
        r#"
[automaton]
name = "Post"
states = ["Unmade", "Posted"]
initial = "Unmade"

[[state]]
name = "thread"
type = "string"
initial = ""

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Posted"
params = ["thread"]

[[action.triggers]]
name = "open_thread"
kind = "entity"
target_entity = "Thread"
target_action = "Open"
guard = "Thread[thread].status == 'Unmade'"

[action.triggers.resolve_target]
kind = "field"
id_field = "thread"
{rule}
"#
    )
}

const THREAD: &str = r#"
[automaton]
name = "Thread"
states = ["Unmade", "Open"]
initial = "Unmade"

[[action]]
name = "Open"
kind = "input"
from = ["Unmade"]
to = "Open"
"#;

#[test]
fn a_trigger_guard_on_a_record_in_scope_is_read_from_it() {
    // The thread is opened whenever the post names one not made yet.
    let rule = "[[invariant]]\nname = \"rule\"\nassert = \"status != 'Unmade' => Thread[thread].status != 'Unmade'\"\n";
    let result = invariant(&[&post(rule), THREAD], None);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_liveness_that_relies_on_a_trigger_guard_on_a_record_in_scope_settles() {
    let rule = "[[liveness]]\nname = \"rule\"\neventually = \"status == 'Posted' => Thread[thread].status != 'Unmade'\"\n";
    let result = liveness(&[&post(rule), THREAD]);
    assert!(result.holds(), "{result}");
}

/// `Go` shuts `B`, then notes `A` once `B` is shut: the runtime reads the
/// second trigger's guard after the first one's reaction has landed.
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
assert = "status == 'Noted' => B[Id].status == 'Open'"
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
fn a_trigger_guard_is_read_after_its_source_commits_not_with_it() {
    // At the commit B is still open, so a guard read then never notes A;
    // read after the first trigger's reaction lands, it does.
    // Only Go comes from outside; the spec's own moves follow.
    let policies = r#"permit(principal, action == Action::"Go", resource);
permit(principal, action, resource) when { context has via && context.via != "request" };"#;
    let result = invariant(&[GO, SHUT], Some(policies));
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

/// `Start` finishes `A` when `C`, a type no rule here names, is on.
const START: &str = r#"
[automaton]
name = "A"
states = ["Idle", "Started", "Done"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Started"

[[action.triggers]]
name = "finish"
kind = "entity"
target_entity = "A"
target_action = "Done"
guard = "C[Id].status == 'On'"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Done"
kind = "internal"
from = ["Started"]
to = "Done"

[[invariant]]
name = "rule"
assert = "status == 'Done' => B[Id].status == 'Full'"
"#;

const BOX: &str = r#"
[automaton]
name = "B"
states = ["Empty", "Full"]
initial = "Empty"

[[action]]
name = "Fill"
kind = "internal"
from = ["Empty"]
to = "Full"
"#;

const SWITCH: &str = r#"
[automaton]
name = "C"
states = ["Off", "On"]
initial = "Off"

[[action]]
name = "Turn"
kind = "input"
from = ["Off"]
to = "On"
"#;

#[test]
fn a_trigger_guard_on_a_type_outside_the_scope_may_hold() {
    // C may be on, and then A is done with the box empty. The walk cannot
    // tell whether it is, so the guard goes both ways: never false, so the
    // rule does not hold, and the way it breaks rests on that guard, so it
    // is not checked, naming the move.
    let result = invariant(&[START, BOX, SWITCH], None);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, JointOutcome::Incomplete { .. }),
        "{text}"
    );
    assert!(text.contains("A.Start"), "{text}");
}

/// `Go` with `k` triggers that each tick `A` again when `C`, a type no rule
/// here names, is on: the model cannot decide any of them.
fn ticks(k: usize) -> String {
    let triggers: String = (0..k)
        .map(|i| {
            format!(
                r#"
[[action.triggers]]
name = "tick{i}"
kind = "entity"
target_entity = "A"
target_action = "Tick"
guard = "C[Id].status == 'On'"

[action.triggers.resolve_target]
kind = "same_id"
"#
            )
        })
        .collect();
    format!(
        r#"
[automaton]
name = "A"
states = ["Idle", "Went"]
initial = "Idle"

[[action]]
name = "Go"
kind = "input"
from = ["Idle"]
to = "Went"
{triggers}
[[action]]
name = "Tick"
kind = "internal"
from = ["Went"]

[[invariant]]
name = "rule"
assert = "status == 'Went' => B[Id].status != 'Full'"
"#
    )
}

#[test]
fn undecided_trigger_guards_build_only_the_ways_the_walk_keeps() {
    // Sixteen undecided guards go 2^16 ways, and all but those with as many
    // ticks in flight as the walk holds are held back: building only those
    // fits a small room, which bounds how many it holds.
    let automatons = parsed(&[&ticks(16), BOX, SWITCH]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let policies = r#"permit(principal, action in [Action::"Go", Action::"Turn"], resource);
permit(principal, action, resource) when { context has via && context.via != "request" };"#;
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(32 << 20), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&refs, Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    let result = results.remove(0);
    assert!(result.holds(), "{result}");
    assert!(
        result.in_flight_bound.is_some_and(|held| held >= 3),
        "{result}"
    );
}

/// `Go` notes `A` when the other `A` it points at is still idle. The walk
/// holds one record of `A`, so the other one is a record it does not hold.
/// `Peek` reads it too, which makes the walk track the pointer.
const POINT: &str = r#"
[automaton]
name = "A"
states = ["Idle", "Went", "Noted"]
initial = "Idle"

[[state]]
name = "other"
type = "string"
initial = ""

[[action]]
name = "Point"
kind = "input"
from = ["Idle"]
to = "Idle"
params = ["other"]

[[action]]
name = "Peek"
kind = "input"
from = ["Idle"]
to = "Idle"
guard = "A[other].status == 'Went'"

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
guard = "A[other].status == 'Idle'"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Note"
kind = "internal"
from = ["Went"]
to = "Noted"

[[invariant]]
name = "rule"
assert = "status != 'Noted' || B[Id].status == 'Gone'"
"#;

const STAY: &str = r#"
[automaton]
name = "B"
states = ["Here", "Gone"]
initial = "Here"

[[action]]
name = "Leave"
kind = "input"
from = ["Here"]
to = "Gone"
"#;

#[test]
fn a_trigger_guard_on_a_record_the_walk_does_not_hold_may_hold() {
    // In the runtime one A points at another that is idle, goes, and is
    // noted while its B is here. The walk's one A is the one that goes: the
    // other it points at is no record the walk holds, so it cannot tell
    // whether the guard holds, and it must not read it as no record.
    let policies = r#"permit(principal, action in [Action::"Point", Action::"Go"], resource is A);
permit(principal, action == Action::"Note", resource is A) when { context has via && context.via == "reaction" };"#;
    let result = invariant(&[POINT, STAY], Some(policies));
    assert!(!result.holds(), "{result}");
}
