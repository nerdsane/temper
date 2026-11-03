//! A rule false only where the walk went by a reaction from a type outside
//! its scope: the walk sends that reaction with made-up parameters, so it
//! walks once more with the type in the scope before it calls it broken.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

/// The one result for `sources`, with `policies` deciding each move.
fn invariant(sources: &[&str], policies: &str) -> JointInvariantResult {
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

/// A note whose `Send` hands the box with its id to `holder`.
fn note(holder: &str) -> String {
    format!(
        r#"
[automaton]
name = "Note"
states = ["Open", "Sent"]
initial = "Open"

[[action]]
name = "Send"
kind = "input"
from = ["Open"]
to = "Sent"

[[action.triggers]]
name = "hand"
kind = "entity"
target_entity = "Box"
target_action = "Hold"

[action.triggers.args]
holder = "'{holder}'"

[action.triggers.resolve_target]
kind = "same_id"
"#
    )
}

/// A box only a reaction holds. Held by anyone but `main`, its lid must be
/// shut, and nothing shuts a lid.
const BOX: &str = r#"
[automaton]
name = "Box"
states = ["Empty", "Held"]
initial = "Empty"

[[state]]
name = "holder"
type = "string"
initial = ""

[[action]]
name = "Hold"
kind = "internal"
from = ["Empty"]
to = "Held"
params = [{ name = "holder", type = "string" }]

[[invariant]]
name = "rule"
assert = "status == 'Held' => holder == 'main' || Lid[Id].status == 'Shut'"
"#;

const LID: &str = r#"
[automaton]
name = "Lid"
states = ["Open", "Shut"]
initial = "Open"

[[action]]
name = "Shut"
kind = "input"
from = ["Open"]
to = "Shut"
"#;

/// Anyone may send a note; only a reaction holds a box; nobody shuts a lid.
const POLICIES: &str = r#"
permit(principal, action == Action::"Send", resource is Note);
permit(principal, action == Action::"Hold", resource is Box) when { context.via == "reaction" };
"#;

#[test]
fn a_rule_only_a_made_up_parameter_from_outside_the_scope_breaks_is_not_checked() {
    // The note hands every box to main, so the rule holds in the runtime.
    // The rule's scope is Box and Lid: the walk sends the note's reaction
    // with any holder, and so finds a box held by someone else. With Note
    // in the scope the holder is main, so it is not broken.
    let result = invariant(&[&note("main"), BOX, LID], POLICIES);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, JointOutcome::Incomplete { .. }),
        "{text}"
    );
    assert!(text.contains("with Note in the scope"), "{text}");
}

#[test]
fn a_rule_the_reaction_from_outside_the_scope_breaks_as_sent_is_broken() {
    // The note hands its box to other while the lid is open: walked again
    // with Note in the scope, sure moves alone break the rule.
    let result = invariant(&[&note("other"), BOX, LID], POLICIES);
    let text = result.to_string();
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected broken, got {text}");
    };
    assert!(
        path.iter().any(|step| step
            .action
            .as_deref()
            .is_some_and(|a| a.starts_with("Note.Send"))),
        "{text}"
    );
}
