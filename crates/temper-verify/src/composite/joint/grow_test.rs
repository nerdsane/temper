//! How many dispatches a walk holds in flight: as many as fit its room. A
//! walk that held a move back walks again with twice as many.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

/// `Go` sends four reactions at once; `B` ends in All only when all four
/// land, in order.
const A: &str = r#"
[automaton]
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

[[action.triggers]]
name = "p4"
kind = "entity"
target_entity = "B"
target_action = "P4"
resolve_target = { kind = "same_id" }
"#;

const B: &str = r#"
[automaton]
name = "B"
states = ["S0", "S1", "S2", "S3", "All"]
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
to = "S3"

[[action]]
name = "P4"
kind = "internal"
from = ["S3"]
to = "All"

[[invariant]]
name = "never_all"
assert = "status != 'All' || A[Id].status == 'Idle'"
"#;

const POLICIES: &str = r#"
permit(principal, action == Action::"Go", resource is A);
permit(principal, action, resource is B) when { context has via && context.via != "request" };
"#;

#[test]
fn a_break_that_needs_four_in_flight_is_found() {
    // With three in flight Go is held back, and the rule seemed to hold.
    let automatons: Vec<Automaton> = [A, B]
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
    let result = results.remove(0);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected broken, got {result}");
    };
    assert_eq!(path.len(), 6, "{result}");
    assert_eq!(result.in_flight_bound, None, "{result}");
}
