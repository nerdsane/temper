//! A joint proof's walk leaves out a move that stays in its place and that
//! nothing acts on, so adding one keeps the proof's digest; a move the
//! kernel acts on stays in the walk.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};
use temper_verify::composite::{
    Bound, Bytes, JointProof, Progress, joint_proofs, verify_joint_invariants,
};

/// A parent that closes once its child has finished.
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
guard = "Child[Id].status == 'Finished'"
"#;

/// A child that finishes, or is late while its parent is Open. A late child
/// never finished, so its parent stays Open; only the parent's `nudge` service may make a
/// finished child late.
const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Finished", "Late"]
initial = "Waiting"

[[action]]
name = "Finish"
kind = "input"
from = ["Waiting"]
to = "Finished"

[[action]]
name = "Hurry"
kind = "input"
from = ["Waiting"]
to = "Late"
guard = "Parent[Id].status == 'Open'"

[[action]]
name = "Nudge"
kind = "internal"
from = ["Finished"]
to = "Late"

[[invariant]]
name = "late_while_open"
assert = "status == 'Late' => Parent[Id].status == 'Open'"
"#;

const CEDAR: &str = r#"
permit(principal, action in [Action::"Close", Action::"Finish", Action::"Hurry", Action::"Remark"], resource);
permit(principal == Agent::"service:nudge", action == Action::"Nudge", resource is Child);
"#;

/// A remark on a Closed parent that writes `memo`, which nothing reads.
const REMARK: &str = r#"
[[state]]
name = "memo"
type = "string"
initial = ""

[[action]]
name = "Remark"
kind = "input"
from = ["Closed"]
params = [{ name = "memo", type = "string" }]
"#;

/// The same remark, which also nudges the child.
const NUDGE: &str = r#"
[[action.triggers]]
name = "nudge"
kind = "entity"
principal = "nudge"
target_entity = "Child"
target_action = "Nudge"

[action.triggers.resolve_target]
kind = "same_id"
"#;

fn parse(sources: &[&str]) -> Vec<Automaton> {
    sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect()
}

fn digests(sources: &[&str], cedar: Option<&str>) -> Vec<String> {
    let automatons = parse(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = cedar.map(|text| SendAnalysis::new(text).expect("fixture policies parse"));
    let room = Bytes::new(1 << 30);
    joint_proofs(&refs, analysis.as_ref(), &room)
        .iter()
        .map(JointProof::digest)
        .collect()
}

/// Whether every joint invariant of `sources` holds under `cedar`.
fn holds(sources: &[&str], cedar: Option<&str>) -> bool {
    let automatons = parse(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = cedar.map(|text| SendAnalysis::new(text).expect("fixture policies parse"));
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let results = verify_joint_invariants(&refs, analysis.as_ref(), bound);
    assert!(!results.is_empty());
    results.iter().all(|result| result.holds())
}

#[test]
fn a_silent_move_keeps_every_joint_digest() {
    let remarked = PARENT.to_string() + REMARK;
    for cedar in [None, Some(CEDAR)] {
        let before = digests(&[PARENT, CHILD], cedar);
        assert_eq!(before.len(), 1);
        assert_eq!(before, digests(&[&remarked, CHILD], cedar), "{cedar:?}");
    }
    assert!(holds(&[PARENT, CHILD], Some(CEDAR)));
    assert!(holds(&[&remarked, CHILD], Some(CEDAR)));
}

/// The remark that nudges the child makes it late while its parent is
/// Closed. The walk must take it to find that.
#[test]
fn a_move_that_sends_a_reaction_is_walked() {
    let nudging = PARENT.to_string() + REMARK + NUDGE;
    assert!(!holds(&[&nudging, CHILD], Some(CEDAR)));
}

#[test]
fn a_move_the_walk_reads_changes_the_joint_digest() {
    let before = digests(&[PARENT, CHILD], Some(CEDAR));
    for (name, remark) in [
        ("a reaction", REMARK.to_string() + NUDGE),
        (
            "a new place",
            REMARK.replace("from = [\"Closed\"]", "from = [\"Closed\"]\nto = \"Open\""),
        ),
        (
            "a guard",
            REMARK.replace(
                "params =",
                "guard = \"Child[Id].status == 'Waiting'\"\nparams =",
            ),
        ),
    ] {
        let parent = PARENT.to_string() + &remark;
        assert_ne!(before, digests(&[&parent, CHILD], Some(CEDAR)), "{name}");
    }
}
