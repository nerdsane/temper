//! One copy in flight is enough of a dispatch its record can take only once
//! (`model/once.rs`): the walk drops a second copy as it is sent.

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

fn invariant(sources: &[&str], policies: &str) -> JointInvariantResult {
    let automatons = parsed(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&refs, Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

/// `Go` may be sent again and again while `A` is idle, each time sending
/// `B` the same `P`; `Close` opens `B`, and only then can `P` land.
const STACKER: &str = r#"[automaton]
name = "A"
states = ["Idle", "Shut"]
initial = "Idle"

[[action]]
name = "Go"
kind = "input"
from = ["Idle"]
to = "Idle"

[[action.triggers]]
name = "step"
kind = "entity"
target_entity = "B"
target_action = "P"
resolve_target = { kind = "same_id" }

[[action]]
name = "Close"
kind = "input"
from = ["Idle"]
to = "Shut"

[[action.triggers]]
name = "open"
kind = "entity"
target_entity = "B"
target_action = "Open"
resolve_target = { kind = "same_id" }
"#;

/// Each `P` steps `B` on, into a status `P` is allowed from again: two
/// copies, both sent before `Close`, take it to S2. With `Close`'s `Open`
/// that is three in flight, all a walk holds at first.
const STEPPER: &str = r#"[automaton]
name = "B"
states = ["Closed", "S0", "S1", "S2"]
initial = "Closed"

[[action]]
name = "Open"
kind = "internal"
from = ["Closed"]
to = "S0"

[[action]]
name = "P"
kind = "internal"
from = ["S0"]
to = "S1"

[[action]]
name = "P"
kind = "internal"
from = ["S1"]
to = "S2"

[[invariant]]
name = "never_last"
assert = "status != 'S2' || A[Id].status == 'Idle'"
"#;

const STACK_POLICIES: &str = r#"permit(principal, action in [Action::"Go", Action::"Close"], resource);
permit(principal, action, resource) when { context has via && context.via != "request" };
"#;

#[test]
fn a_break_that_needs_two_identical_copies_in_flight_is_found() {
    // Each P lands into a status P is allowed from, so a record takes it
    // again: both copies count, and the walk keeps both.
    let result = invariant(&[STACKER, STEPPER], STACK_POLICIES);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

/// `Looked` may be sent again and again while `R` is looking, each time
/// sending it the same `Propose`, which it takes once: from Proposed it never
/// looks again.
const PROPOSER: &str = r#"[automaton]
name = "R"
states = ["New", "Looking", "Proposed", "Done"]
initial = "New"

[[action]]
name = "Look"
kind = "input"
from = ["New"]
to = "Looking"

[[action]]
name = "Looked"
kind = "input"
from = ["Looking", "Proposed", "Done"]

[[action.triggers]]
name = "propose"
kind = "entity"
target_entity = "R"
target_action = "Propose"
resolve_target = { kind = "same_id" }
guard = "status == 'Looking'"

[[action]]
name = "Propose"
kind = "internal"
from = ["Looking"]
to = "Proposed"

[[action]]
name = "Finish"
kind = "internal"
from = ["Proposed"]
to = "Done"

[[action]]
name = "Lost"
kind = "internal"
from = ["Looking"]
to = "Done"

[[state_timeout]]
state = "Looking"
after_seconds = 2700
on_timeout = "Lost"

[[state_timeout]]
state = "Proposed"
after_seconds = 60
on_timeout = "Finish"

[[liveness]]
name = "LookEnds"
eventually = "status != 'Looking'"
"#;

const PROPOSE_POLICIES: &str = r#"permit(principal, action in [Action::"Look", Action::"Looked"], resource);
permit(principal, action, resource) when { context has via && context.via != "request" };
"#;

#[test]
fn a_dispatch_taken_once_needs_one_copy_in_flight() {
    // Before, each Looked stacked one more Propose, and the walk grew until
    // its room ran out. One copy is kept: nothing is held back.
    let automatons = parsed(&[PROPOSER]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(PROPOSE_POLICIES).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&refs, Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    let result = results.remove(0);
    assert!(result.holds(), "{result}");
    assert_eq!(result.in_flight_bound, None, "{result}");
}

/// `Go` sends three different steps at once; `B` takes each once.
const THREE: &str = r#"[automaton]
name = "A"
states = ["Idle", "Went"]
initial = "Idle"

[[action]]
name = "Go"
kind = "input"
from = ["Idle"]
to = "Went"

[[action.triggers]]
name = "p1"
kind = "entity"
target_entity = "B"
target_action = "P1"
resolve_target = { kind = "same_id" }

[[action.triggers]]
name = "p2"
kind = "entity"
target_entity = "B"
target_action = "P2"
resolve_target = { kind = "same_id" }

[[action.triggers]]
name = "p3"
kind = "entity"
target_entity = "B"
target_action = "P3"
resolve_target = { kind = "same_id" }

"#;

const STEPS: &str = r#"[automaton]
name = "B"
states = ["S0", "S1", "S2", "All"]
initial = "S0"

[[action]]
name = "P1"
kind = "internal"
from = ["S0"]
to = "S1"

[[action]]
name = "P2"
kind = "internal"
from = ["S1"]
to = "S2"

[[action]]
name = "P3"
kind = "internal"
from = ["S2"]
to = "All"

[[invariant]]
name = "never_all"
assert = "status != 'All' || A[Id].status == 'Idle'"
"#;

const THREE_POLICIES: &str = r#"permit(principal, action == Action::"Go", resource is A);
permit(principal, action, resource is B) when { context has via && context.via != "request" };
"#;

#[test]
fn three_different_dispatches_in_flight_still_break_the_rule() {
    // Each step is taken once, but the three are not copies of one another:
    // all three stay in flight.
    let result = invariant(&[THREE, STEPS], THREE_POLICIES);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

/// `Go` may be sent again and again while `A` is idle, each time sending
/// `B` the same `Propose`; `Stop` sends `B` the `Ready` that `Propose`'s
/// guard waits for.
const READIER: &str = r#"[automaton]
name = "A"
states = ["Idle", "Shut", "Never"]
initial = "Idle"

[[action]]
name = "Go"
kind = "input"
from = ["Idle"]
to = "Idle"

[[action.triggers]]
name = "propose"
kind = "entity"
target_entity = "B"
target_action = "Propose"
resolve_target = { kind = "same_id" }

[[action]]
name = "Stop"
kind = "input"
from = ["Idle"]
to = "Shut"

[[action.triggers]]
name = "ready"
kind = "entity"
target_entity = "B"
target_action = "Ready"
resolve_target = { kind = "same_id" }
"#;

/// `Propose` is taken once, but refused while `ready` is false: a copy that
/// lands before `Ready` leaves `B` looking, still able to take the next.
const WAITER: &str = r#"[automaton]
name = "B"
states = ["Looking", "Proposed"]
initial = "Looking"

[[state]]
name = "ready"
type = "bool"
initial = false

[[action]]
name = "Propose"
kind = "internal"
from = ["Looking"]
to = "Proposed"
guard = "ready"

[[action]]
name = "Ready"
kind = "internal"
from = ["Looking"]
to = "Looking"
effect = ["ready = true"]

[[invariant]]
name = "never_proposed"
assert = "status != 'Proposed' || !ready || A[Id].status == 'Never'"
"#;

const READY_POLICIES: &str = r#"permit(principal, action in [Action::"Go", Action::"Stop"], resource);
permit(principal, action, resource) when { context has via && context.via != "request" };
"#;

#[test]
fn a_copy_refused_while_its_record_still_takes_it_leaves_one_to_land() {
    // A run may land one copy before Ready, refused, and a second after it,
    // taken. The walk's one copy waits in flight for Ready and lands then.
    let result = invariant(&[READIER, WAITER], READY_POLICIES);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

/// Each `Go` spawns the same `Run` with `Start`, storing its id in `run`.
const SPAWNER: &str = r#"[automaton]
name = "Job"
states = ["Empty"]
initial = "Empty"

[[state]]
name = "run"
type = "string"
initial = ""

[[state]]
name = "kid"
type = "string"
initial = "r1"

[[state]]
name = "p"
type = "string"
initial = "x"

[[action]]
name = "Go"
kind = "input"
from = ["Empty"]
to = "Empty"
effect = ["spawn('Run', 'Start', run, 'r1', copy(p))"]

[[invariant]]
name = "started_is_named"
assert = "Run[kid].status != 'Going' || run != ''"
"#;

/// `Start` is taken once, and refused until `Tag` sets the field its
/// parameter must equal.
const SPAWNED: &str = r#"[automaton]
name = "Run"
states = ["New", "Going"]
initial = "New"

[[state]]
name = "tag"
type = "string"
initial = ""

[[action]]
name = "Start"
kind = "input"
from = ["New"]
to = "Going"
params = [{ name = "p", type = "string" }]

[[action.constraints]]
kind = "param_equals_field"
param = "p"
field = "tag"

[[action]]
name = "Tag"
kind = "input"
from = ["New"]
to = "New"
effect = ["tag = 'x'"]
"#;

const SPAWN_POLICIES: &str =
    r#"permit(principal, action in [Action::"Go", Action::"Tag"], resource);"#;

#[test]
fn a_refused_first_action_of_a_spawn_keeps_every_copy() {
    // Two Go send two Starts; one is refused before Tag, which puts `run`
    // back, and the other lands after it: the run is Going and unnamed.
    let result = invariant(&[SPAWNER, SPAWNED], SPAWN_POLICIES);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}
