//! Made-up values are kept apart by class: what nothing compares need not be
//! equal, and what Cedar compares still can be.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::*;

/// A room with two notes no spec compares (its rule reads each, so the walk
/// keeps them), and a door whose knock fills it, which breaks the rule: the
/// door never opens. Cedar decides a reaction from what the record holds, so
/// whether the room fills turns on the two notes.
const ROOM: &str = r#"
[automaton]
name = "Room"
states = ["Empty", "Full"]
initial = "Empty"

[[state]]
name = "left"
type = "string"
initial = ""

[[state]]
name = "right"
type = "string"
initial = ""

[[action]]
name = "Left"
kind = "input"
from = ["Empty"]
params = [{ name = "left", type = "string" }]

[[action]]
name = "Right"
kind = "input"
from = ["Empty"]
params = [{ name = "right", type = "string" }]

[[action]]
name = "Fill"
kind = "input"
from = ["Empty"]
to = "Full"

[[invariant]]
name = "FullBehindAnOpenDoor"
assert = "status == 'Empty' || (Door[Id].status == 'Open' && left != '' && right != '')"
"#;

const DOOR: &str = r#"
[automaton]
name = "Door"
states = ["Shut", "Open"]
initial = "Shut"

[[action]]
name = "Knock"
kind = "input"
from = ["Shut"]
to = "Shut"

[[action.triggers]]
name = "fill"
kind = "entity"
principal = "tell"
target_entity = "Room"
target_action = "Fill"

[action.triggers.resolve_target]
kind = "same_id"
"#;

fn check(policies: &str) -> JointInvariantResult {
    let automatons: Vec<Automaton> = [ROOM, DOOR]
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

#[test]
fn values_cedar_compares_can_be_equal_though_no_spec_compares_them() {
    let result = check(
        r#"
        permit(principal, action in [Action::"Left", Action::"Right", Action::"Knock"], resource);
        permit(principal, action == Action::"Fill", resource)
            when { context has via && context.via != "request" && resource.left != "" && resource.left == resource.right };
        "#,
    );
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}
