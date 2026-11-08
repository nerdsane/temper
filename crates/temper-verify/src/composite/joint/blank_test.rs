//! A string a reaction passes to a target that only requires it not blank:
//! the walk carries whether it is blank, not which value it is.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

/// A note whose Send asks its box to Take, passing its text as the reason.
/// `write_from` is where the note may set its text, if anywhere.
fn note(write_from: Option<&str>) -> String {
    let write = write_from.map_or_else(String::new, |from| {
        format!(
            "[[action]]\nname = \"Write\"\nkind = \"input\"\nfrom = {from}\n\
             params = [{{ name = \"text\", type = \"string\" }}]\n\n"
        )
    });
    format!(
        r#"
[automaton]
name = "Note"
states = ["Open", "Sent"]
initial = "Open"

[[state]]
name = "text"
type = "string"
initial = ""

{write}[[action]]
name = "Send"
kind = "input"
from = ["Open"]
to = "Sent"

[[action.triggers]]
name = "take"
kind = "entity"
target_entity = "Box"
target_action = "Take"
resolve_target = {{ kind = "same_id" }}

[action.triggers.args]
reason = "text"
"#
    )
}

/// A box that takes only a reason that is not blank, and holds none.
fn boxed(rule: &str) -> String {
    format!(
        r#"
[automaton]
name = "Box"
states = ["Empty", "Taken"]
initial = "Empty"

[[action]]
name = "Take"
kind = "internal"
from = ["Empty"]
to = "Taken"

[[action.constraints]]
kind = "param_nonempty"
param = "reason"

[[action.params]]
name = "reason"
type = "string"

[[invariant]]
name = "rule"
assert = "{rule}"
"#
    )
}

const POLICIES: &str = r#"
permit(principal, action in [Action::"Write", Action::"Send"], resource is Note);
permit(principal, action == Action::"Take", resource is Box) when { context has via && context.via != "request" };
"#;

const NEVER_TAKEN: &str = "status != 'Taken' || Note[Id].status == 'Open'";

fn check(sources: &[String]) -> JointInvariantResult {
    let automatons: Vec<Automaton> = sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(POLICIES).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&refs, Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn a_reason_the_box_only_requires_not_blank_splits_no_state_by_its_value() {
    // The note may rewrite its text while the Take is in flight: the walk
    // tells apart a blank reason from one that is not, and no more.
    let rule = "status == 'Taken' => Note[Id].status == 'Sent'";
    let result = check(&[note(Some(r#"["Open", "Sent"]"#)), boxed(rule)]);
    assert!(result.holds(), "{result}");
    assert!(result.states <= 21, "{result}");
}

#[test]
fn a_reason_that_is_only_ever_blank_is_refused() {
    let result = check(&[note(None), boxed(NEVER_TAKEN)]);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_reason_that_is_not_blank_is_taken() {
    let result = check(&[note(Some(r#"["Open", "Sent"]"#)), boxed(NEVER_TAKEN)]);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}
