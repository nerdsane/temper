use super::*;

/// A note, addressed when it is for someone: its Add's own reaction makes
/// Address, as whoever added it.
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

/// Members add notes; Address follows only from a reaction; nothing is
/// allowed to a visitor.
const MEMBERS_ADD: &str = r#"
permit(principal, action == Action::"Add", resource is Note)
when { principal has role && principal.role == "member" };
permit(principal, action == Action::"Address", resource is Note)
when { context has via && context.via != "request" };
forbid(principal, action, resource is Note)
when { principal has role && principal.role == "visitor" };
"#;

fn check(policies: &str) -> JointLivenessResult {
    let note = parse_automaton(NOTE).expect("fixture parses");
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&[&note], Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn a_chain_runs_as_the_one_principal_who_sent_its_request() {
    // Only a member may Add, and Cedar refuses Address only to a visitor,
    // who may not Add: no sender's Add is followed by a refused Address.
    let result = check(MEMBERS_ADD);
    assert!(result.holds(), "{result}");
}

/// Members and visitors add notes, and Cedar refuses Address to a visitor.
const VISITORS_ADD: &str = r#"
permit(principal, action == Action::"Add", resource is Note)
when { principal has role && (principal.role == "member" || principal.role == "visitor") };
permit(principal, action == Action::"Address", resource is Note)
when { context has via && context.via != "request" };
forbid(principal, action == Action::"Address", resource is Note)
when { principal has role && principal.role == "visitor" };
"#;

#[test]
fn a_chain_cedar_refuses_its_own_sender_halfway_never_settles() {
    // A visitor's Add goes through and its Address is refused, so its note
    // stays Posted.
    let result = check(VISITORS_ADD);
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Note=Posted")),
        "{result}"
    );
}

#[test]
fn a_non_string_like_error_must_not_hide_a_refused_reaction() {
    let result = check(
        r#"
permit(principal, action == Action::"Add", resource is Note)
when { principal has flag };
forbid(principal, action == Action::"Add", resource is Note)
when { !(principal.flag like "x*") };
permit(principal, action == Action::"Address", resource is Note)
when { principal.flag like "x*" };
"#,
    );
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Note=Posted")),
        "{result}"
    );
}

#[test]
fn a_chain_keeps_a_sender_selected_by_combined_checks() {
    let result = check(
        r#"
permit(principal, action == Action::"Add", resource is Note)
when { (principal has role) == (principal has agent_type) };
permit(principal, action == Action::"Address", resource is Note)
when { (principal has role) == (principal has agent_type) && context.via != "request" };
"#,
    );
    assert!(result.holds(), "{result}");
}

#[test]
fn a_chain_keeps_its_sender_until_the_reaction_rule_is_known() {
    let policies = MEMBERS_ADD
        .replace(
            "context has via && context.via != \"request\"",
            "context.rule == \"Note:Add:address\"",
        )
        .replace(
            "principal.role == \"visitor\"",
            "principal.role == \"visitor\" && context.rule == \"Note:Add:address\"",
        );
    let result = check(&policies);
    assert!(result.holds(), "{result}");
}

#[test]
fn different_senders_stay_apart_until_the_reaction_rule_is_known() {
    let result = check(
        r#"
permit(principal, action == Action::"Add", resource is Note)
when { principal has role && principal.role in ["member", "visitor"] };
permit(principal, action == Action::"Address", resource is Note)
when { principal has role && principal.role == "member" && context.rule == "Note:Add:address" };
permit(principal, action == Action::"Address", resource is Note)
when { principal has role && principal.role == "visitor" && context.rule == "Note:Add:other" };
"#,
    );
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Note=Posted")),
        "{result}"
    );
}

#[test]
fn sender_construction_stops_incomplete_when_its_room_is_full() {
    // With no construction accounting this parity expression enumerates
    // 2^24 profiles before the walk even starts.
    let condition = (1..24).fold("(principal has flag0)".to_string(), |left, at| {
        format!("({left} == (principal has flag{at}))")
    });
    let policies = SendAnalysis::new(&format!(
        r#"
permit(principal, action == Action::"Add", resource is Note) when {{ {condition} }};
permit(principal, action == Action::"Address", resource is Note)
when {{ {condition} && context.via != "request" }};
"#
    ))
    .expect("fixture policies parse");
    let note = parse_automaton(NOTE).expect("fixture parses");
    let room = Bytes::new(1 << 20);
    let progress = Progress::default();
    let results = verify_joint_liveness(
        &[&note],
        Some(&policies),
        Bound {
            room: &room,
            progress: &progress,
        },
    );
    assert_eq!(results.len(), 1);
    assert!(
        matches!(&results[0].outcome, LivenessOutcome::Incomplete { reason } if reason.contains("sender-kind construction")),
        "{}",
        results[0]
    );
    assert_eq!(results[0].states, 0);
    assert_eq!(
        room.held(),
        0,
        "a failed construction releases every reservation"
    );
}

#[test]
fn a_proof_keeps_its_senders_charged_until_the_proof_is_dropped() {
    let note = parse_automaton(NOTE).expect("fixture parses");
    let policies = SendAnalysis::new(MEMBERS_ADD).expect("fixture policies parse");
    let room = Bytes::new(64 << 20);
    let proofs = liveness_proofs(&[&note], Some(&policies), &room);
    let retained = room.held();
    assert!(
        retained > 0,
        "sender profiles remain charged between construction and walking"
    );
    let progress = Progress::default();
    assert!(
        proofs[0]
            .check(Bound {
                room: &room,
                progress: &progress
            })
            .holds()
    );
    assert_eq!(room.held(), retained, "the walk releases its own room");
    drop(proofs);
    assert_eq!(room.held(), 0);
}

#[test]
fn a_proof_keeps_answer_tables_charged_without_a_sender_kind_split() {
    let note = parse_automaton(NOTE).expect("fixture parses");
    let policies =
        SendAnalysis::new("permit(principal, action, resource);").expect("fixture policies parse");
    let room = Bytes::new(64 << 20);
    let proofs = liveness_proofs(&[&note], Some(&policies), &room);
    let retained = room.held();
    assert!(retained > 0, "unsplit senders still retain answer tables");
    let progress = Progress::default();
    assert!(
        proofs[0]
            .check(Bound {
                room: &room,
                progress: &progress
            })
            .holds()
    );
    assert_eq!(room.held(), retained, "walking retains the tables' charge");
    drop(proofs);
    assert_eq!(room.held(), 0, "dropping the proof releases its tables");
}
