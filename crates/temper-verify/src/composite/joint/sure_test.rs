//! An invariant is broken only where moves the runtime surely can make lead.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::*;

/// A parent whose `Close` tells its child to finish, and whose `Leave`
/// follows it; `way` is a path of outside moves `(action, state)` from Open
/// to Closed that tells the child nothing.
fn parent(way: &[(&str, &str)]) -> String {
    let mut states = vec!["Open"];
    states.extend(way.iter().map(|(_, to)| *to).filter(|to| *to != "Closed"));
    states.extend(["Closed", "Gone"]);
    let states: Vec<String> = states.iter().map(|s| format!("\"{s}\"")).collect();
    let mut source = format!(
        r#"
[automaton]
name = "Parent"
states = [{}]
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
name = "Leave"
kind = "input"
from = ["Closed"]
to = "Gone"
"#,
        states.join(", ")
    );
    let mut at = "Open";
    for (action, to) in way {
        source.push_str(&format!(
            "\n[[action]]\nname = \"{action}\"\nkind = \"input\"\nfrom = [\"{at}\"]\nto = \"{to}\"\n"
        ));
        at = to;
    }
    source
}

/// A child that finishes when its parent tells it to. Its `note` is always
/// "fine", and no guard or rule reads it, so the walk does not hold it.
const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Done"]
initial = "Waiting"

[[state]]
name = "note"
type = "string"
initial = "fine"

[[action]]
name = "Finish"
kind = "internal"
from = ["Waiting"]
to = "Done"

[[invariant]]
name = "finished"
assert = "Parent[Id].status != 'Gone' || status == 'Done'"
"#;

/// Cedar lets the parent's reaction finish the child when its note is
/// "fine", which it always is: the walk, not holding the note, cannot tell.
const FINISH_WHEN_FINE: &str = r#"
permit(principal, action, resource is Parent);
permit(principal, action == Action::"Finish", resource is Child) when {
    context.via == "reaction" && resource.note == "fine"
};
"#;

fn check(way: &[(&str, &str)]) -> JointInvariantResult {
    check_under(&[parent(way), CHILD.to_string()], FINISH_WHEN_FINE)
}

fn check_under(sources: &[String], policies: &str) -> JointInvariantResult {
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

fn refused_by_cedar(path: &[JointStep]) -> bool {
    path.iter().any(|step| {
        step.action
            .as_deref()
            .is_some_and(|action| action.contains("refused by Cedar"))
    })
}

#[test]
fn a_break_only_where_cedar_may_refuse_is_not_checked() {
    // The child is left waiting only if Cedar refuses its finish, which the
    // real policy never does. Its answer turns on a field alone, so no caller
    // can bring the refusal about: it says it cannot check it, and names that move.
    let result = check(&[]);
    let JointOutcome::Incomplete { reason } = &result.outcome else {
        panic!("expected not checked, got {result}");
    };
    assert!(reason.contains("Child.Finish refused by Cedar"), "{result}");
}

#[test]
fn a_break_sure_moves_reach_stays_broken() {
    // Dropping the parent closes it without telling the child.
    let result = check(&[("Drop", "Closed")]);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected broken, got {result}");
    };
    assert!(!refused_by_cedar(path), "{result}");
}

#[test]
fn a_break_reached_first_by_an_unsure_move_is_broken_by_the_sure_way() {
    // The walk first reaches the closed parent and waiting child through
    // Cedar's refusal, and walks on from there, before the longer sure way
    // reaches them too: the sure way must still be found and reported.
    let way = [
        ("Prepare", "Ready"),
        ("Arm", "Armed"),
        ("Lock", "Locked"),
        ("Drop", "Closed"),
    ];
    let result = check(&way);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected broken, got {result}");
    };
    assert!(!refused_by_cedar(path), "{result}");
    assert!(
        path.iter().any(|step| step
            .action
            .as_deref()
            .is_some_and(|a| a.contains("Parent.Drop"))),
        "{result}"
    );
}

#[test]
fn a_reaction_guard_outside_the_proof_does_not_make_a_break_sure() {
    // The invariant scopes the proof to Parent and Child. Gate is declared
    // and permanently closed, but outside that scope, so no Read runs for
    // Finish. Its arrival must stay undecided, rather than assume Gate was
    // read and allowed it. The runtime can never reach the reported break.
    let child = CHILD
        .replace(
            "to = \"Done\"",
            "to = \"Done\"\nguard = \"Gate[Id].status == 'Open'\"",
        )
        .replace(
            "Parent[Id].status != 'Gone' || status == 'Done'",
            "status != 'Done' || Parent[Id].status != 'Closed'",
        );
    let gate = r#"
[automaton]
name = "Gate"
states = ["Closed"]
initial = "Closed"
"#;
    let policies = r#"
permit(principal, action, resource is Parent);
permit(principal, action == Action::"Finish", resource is Child) when {
    context.via == "reaction"
};
"#;
    let result = check_under(&[parent(&[]), child, gate.to_string()], policies);
    let JointOutcome::Incomplete { reason } = &result.outcome else {
        panic!("expected not checked, got {result}");
    };
    assert!(reason.contains("Child.Finish arrives"), "{result}");
}
