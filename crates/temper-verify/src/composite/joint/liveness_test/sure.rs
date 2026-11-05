//! A stuck verdict refuses an install, so it says only what the runtime can
//! do: sure moves alone lead to the state, and no move can lead out of it.

use super::*;

/// A note whose Add's own reaction makes Address, as whoever added it.
const NOTE: &str = r#"
[automaton]
name = "Note"
states = ["Unmade", "Posted", "Waiting"]
initial = "Unmade"

[[state]]
name = "to"
type = "string"
initial = ""

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Posted"
params = [{ name = "to", type = "string" }]

[[action.triggers]]
name = "address"
kind = "entity"
target_entity = "Note"
target_action = "Address"
guard = "to != ''"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Address"
kind = "input"
from = ["Posted"]
to = "Waiting"

[[liveness]]
name = "AddressedNoteWaits"
eventually = "to == '' || status != 'Posted'"
"#;

/// A type outside the rule's scope whose move adds a note.
const PUSH: &str = r#"
[automaton]
name = "Push"
states = ["Idle", "Sent"]
initial = "Idle"

[[action]]
name = "Send"
kind = "input"
from = ["Idle"]
to = "Sent"

[[action.triggers]]
name = "note"
kind = "entity"
target_entity = "Note"
target_action = "Add"

[action.triggers.args]
to = "'hub'"

[action.triggers.resolve_target]
kind = "same_id"
"#;

/// Notes come only from reactions, and Cedar refuses a guest everything on a
/// note and on a push, so no guest's chain adds one.
const FROM_REACTIONS: &str = r#"
permit(principal, action in [Action::"Add", Action::"Address"], resource is Note)
when { context has via && context.via != "request" };
permit(principal, action == Action::"Send", resource is Push);
forbid(principal, action, resource is Note)
when { principal has role && principal.role like "guest*" };
forbid(principal, action, resource is Push)
when { principal has role && principal.role like "guest*" };
"#;

/// A job a timer finishes when a gate the rule's scope leaves out is open.
const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Unmade", "Running", "Done"]
initial = "Unmade"

[[state]]
name = "gate"
type = "string"
initial = ""

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Running"
params = [{ name = "gate", type = "string" }]

[[action]]
name = "Finish"
kind = "internal"
from = ["Running"]
to = "Done"
guard = "Gate[gate].status == 'Open'"

[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "Finish"

[[liveness]]
name = "RunEnds"
eventually = "status != 'Running'"
"#;

const GATE: &str = r#"
[automaton]
name = "Gate"
states = ["Unmade", "Open"]
initial = "Unmade"

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Open"
"#;

fn check(specs: &[&str], policies: Option<&str>) -> JointLivenessResult {
    let automatons: Vec<Automaton> = specs
        .iter()
        .map(|spec| parse_automaton(spec).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = policies.map(|p| SendAnalysis::new(p).expect("fixture policies parse"));
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&refs, analysis.as_ref(), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn a_reaction_from_outside_the_scope_runs_as_whoever_may_start_its_chain() {
    // Only Push's reaction adds a note, and it runs as whoever sent Send:
    // Cedar refuses Address only to a guest, who may not Send, so every
    // added note is addressed.
    let result = check(&[NOTE, PUSH], Some(FROM_REACTIONS));
    assert!(result.holds(), "{result}");
}

#[test]
fn a_state_a_move_the_model_cannot_decide_may_leave_is_not_stuck() {
    // Sure moves reach Running, and its timer may finish it: the gate is
    // outside the rule's scope, so the model cannot say it ever is open.
    let result = check(&[JOB, GATE], None);
    let LivenessOutcome::Incomplete { reason } = &result.outcome else {
        panic!("{result}");
    };
    assert!(reason.contains("Job"), "{result}");
}

#[test]
fn a_chain_cedar_surely_refuses_on_a_sure_way_is_still_stuck() {
    // A visitor's Add is sure, and so is Cedar's refusal of its Address.
    let result = check(
        &[NOTE],
        Some(
            r#"
permit(principal, action == Action::"Add", resource is Note)
when { principal has role && (principal.role == "member" || principal.role == "visitor") };
permit(principal, action == Action::"Address", resource is Note)
when { context has via && context.via != "request" };
forbid(principal, action == Action::"Address", resource is Note)
when { principal has role && principal.role == "visitor" };
"#,
        ),
    );
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{result}"
    );
}
