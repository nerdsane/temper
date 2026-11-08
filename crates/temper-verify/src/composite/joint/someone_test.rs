//! A request is sure when some real sender is definitely allowed it, and
//! stays unsure when Cedar cannot decide it for anyone.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::*;

/// A room that should only fill behind an open door, and a door nothing ties
/// to it: filling first breaks the rule, if anyone may fill.
const ROOM: &str = r#"
[automaton]
name = "Room"
states = ["Empty", "Full"]
initial = "Empty"

[[action]]
name = "Fill"
kind = "input"
from = ["Empty"]
to = "Full"

[[invariant]]
name = "FullBehindAnOpenDoor"
assert = "status == 'Empty' || Door[Id].status == 'Open'"
"#;

const DOOR: &str = r#"
[automaton]
name = "Door"
states = ["Shut", "Open"]
initial = "Shut"

[[action]]
name = "Unlock"
kind = "input"
from = ["Shut"]
to = "Open"
"#;

/// A parent whose `Close` tells its child to finish as whoever sent it, and
/// whose `Drop` closes it without telling the child.
const PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Closed"]
initial = "Open"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action.triggers]]
name = "finish"
kind = "entity"
target_entity = "Child"
target_action = "Finish"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Drop"
kind = "input"
from = ["Open"]
to = "Closed"

[[action]]
name = "Note"
kind = "input"
from = ["Open"]
to = "Open"
"#;

const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Done"]
initial = "Waiting"

[[action]]
name = "Finish"
kind = "internal"
from = ["Waiting"]
to = "Done"

[[invariant]]
name = "Finished"
assert = "Parent[Id].status != 'Closed' || status == 'Done'"
"#;

fn check(sources: &[&str], policies: &str) -> JointInvariantResult {
    let automatons: Vec<Automaton> = sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
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

fn takes(path: &[JointStep], move_name: &str) -> bool {
    path.iter().any(|step| {
        step.action
            .as_deref()
            .is_some_and(|action| action.contains(move_name))
    })
}

#[test]
fn a_move_one_real_sender_may_make_can_break_a_rule() {
    // The owner may do everything, and whether anyone else may depends on who
    // they are. A sender with a definite yes exists, so the break is real.
    let result = check(
        &[ROOM, DOOR],
        r#"permit(principal is Customer, action, resource) when { principal.id == "owner" };"#,
    );
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected broken, got {result}");
    };
    assert!(takes(path, "Room.Fill"), "{result}");
}

#[test]
fn a_move_allowed_one_of_the_senders_it_stands_for_can_break_a_rule() {
    // Closer and dropper answer the child's reaction alike, so the model sends
    // both as one sender. Only the dropper may drop: that sender has a
    // definite yes, whatever the closer may do.
    let result = check(
        &[PARENT, CHILD],
        r#"
permit(principal, action == Action::"Close", resource is Parent) when { principal.id == "closer" };
permit(principal, action == Action::"Drop", resource is Parent) when { principal.id == "dropper" };
permit(principal, action == Action::"Note", resource is Parent) when { principal.id == "noter" };
permit(principal, action == Action::"Finish", resource is Child) when {
    context.via == "reaction" && !(principal.id == "noter")
};
"#,
    );
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected broken, got {result}");
    };
    assert!(takes(path, "Parent.Drop"), "{result}");
}

/// The rule must be reported as broken if Cedar allows `Fill`, and no more.
fn breaks_if_cedar_allows_fill(policies: &str) {
    let result = check(&[ROOM, DOOR], policies);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
    let text = result.to_string();
    assert!(text.contains("BROKEN if"), "{text}");
    assert!(text.contains("Room.Fill"), "{text}");
}

#[test]
fn a_permit_that_turns_on_the_record_stays_unsure() {
    // The walk does not hold the note, so Cedar's answer is open for everyone.
    breaks_if_cedar_allows_fill(
        r#"
permit(principal is Customer, action, resource) when {
    principal.id == "owner" && resource has note && resource.note == "fine"
};
"#,
    );
}

#[test]
fn a_forbid_that_turns_on_the_record_stays_unsure() {
    // The owner is permitted, but a forbid nobody can rule out may still
    // apply to the very sender who is permitted.
    breaks_if_cedar_allows_fill(
        r#"
permit(principal is Customer, action, resource) when { principal.id == "owner" };
forbid(principal, action, resource) when { resource has frozen && resource.frozen == true };
"#,
    );
}

#[test]
fn no_principal_the_kernel_cannot_build_makes_a_move_sure() {
    // The kernel builds only customers, agents and admins for someone outside.
    breaks_if_cedar_allows_fill(
        r#"
permit(principal, action, resource) when {
    !(principal is Customer) && !(principal is Agent) && !(principal is Admin)
};
"#,
    );
}

#[test]
fn a_principal_type_the_kernel_never_builds_makes_no_move_sure() {
    // A policy may still name a type nothing can be. Who else may is open.
    breaks_if_cedar_allows_fill(
        r#"
permit(principal is Robot, action, resource);
permit(principal, action == Action::"Fill", resource is Room) when {
    principal has role && principal.role == "filler" && resource has note && resource.note == "fine"
};
"#,
    );
}

/// A room whose `Fill` unlocks its door as `triggers` say. `extra` is more of
/// the room's spec.
fn room_with(fill: &str, extra: &str) -> String {
    format!(
        r#"
[automaton]
name = "Room"
states = ["Empty", "Full"]
initial = "Empty"

[[state]]
name = "kind"
type = "string"
initial = ""

[[action]]
name = "Fill"
kind = "input"
from = ["Empty"]
to = "Full"
{fill}

[[invariant]]
name = "FullBehindAnOpenDoor"
assert = "status == 'Empty' || Door[Id].status == 'Open'"
{extra}
"#
    )
}

fn unlock_when(guard: &str, name: &str) -> String {
    format!(
        r#"
[[action.triggers]]
name = "{name}"
kind = "entity"
target_entity = "Door"
target_action = "Unlock"
guard = "{guard}"

[action.triggers.resolve_target]
kind = "same_id"
"#
    )
}

const OWNER_AND_UNLOCK: &str = r#"
permit(principal is Customer, action, resource) when { principal.id == "owner" };
permit(principal, action == Action::"Unlock", resource is Door) when { context.via == "reaction" };
"#;

#[test]
fn a_trigger_guard_on_the_requests_own_parameter_is_the_senders_choice() {
    // The model holds no `kind`, yet the owner may send any: a fill that is
    // not urgent leaves the door shut, as surely as anything a sender chooses.
    let fill = format!(
        "params = [\"kind\"]\n{}",
        unlock_when("kind == 'urgent'", "urgent")
    );
    let result = check(&[&room_with(&fill, ""), DOOR], OWNER_AND_UNLOCK);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected broken, got {result}");
    };
    assert!(takes(path, "Room.Fill"), "{result}");
}

#[test]
fn a_trigger_guard_on_a_field_an_earlier_move_set_stays_unsure() {
    // Whoever filled did not choose `kind` on this move, and the model does
    // not hold what `Mark` left there.
    let extra = r#"
[[action]]
name = "Mark"
kind = "input"
from = ["Empty"]
params = ["kind"]
"#;
    let fill = unlock_when("kind == 'urgent'", "urgent");
    stays_not_checked_with(&room_with(&fill, extra));
}

#[test]
fn two_trigger_guards_on_one_parameter_are_not_two_choices() {
    // One of them always fires, so the door always unlocks. Taking each as
    // its own choice would make up a fill that unlocks nothing.
    let fill = format!(
        "params = [\"kind\"]\n{}{}",
        unlock_when("kind == 'urgent'", "urgent"),
        unlock_when("kind != 'urgent'", "calm"),
    );
    stays_not_checked_with(&room_with(&fill, ""));
}

#[test]
fn a_trigger_guard_the_sender_can_only_make_hold_is_no_choice() {
    // `kind` may not be empty, so the guard always holds and the door always
    // unlocks. The sender cannot leave it shut.
    let fill = format!(
        "params = [\"kind\"]\n{}\n[[action.constraints]]\nkind = \"param_nonempty\"\nparam = \"kind\"\n",
        unlock_when("kind != ''", "any"),
    );
    stays_not_checked_with(&room_with(&fill, ""));
}

fn stays_not_checked_with(room: &str) {
    let result = check(&[room, DOOR], OWNER_AND_UNLOCK);
    let JointOutcome::Incomplete { reason } = &result.outcome else {
        panic!("expected not checked, got {result}");
    };
    assert!(reason.contains("Room.Fill"), "{result}");
}
