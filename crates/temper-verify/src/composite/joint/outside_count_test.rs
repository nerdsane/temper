//! A count carried in by a reaction from a type outside a rule's scope.

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

/// `Src`, outside the scope, carries its count to `A`, which takes it only
/// when it is newer and passes it on to `B`, which takes it the same way.
const SOURCE: &str = r#"
[automaton]
name = "Src"
states = ["Ready"]
initial = "Ready"

[[state]]
name = "n"
type = "counter"
initial = 0

[[action]]
name = "Bump"
kind = "input"
from = ["Ready"]
to = "Ready"
effect = ["n += 1"]

[[action.triggers]]
name = "copy"
kind = "entity"
target_entity = "A"
target_action = "Copy"
[action.triggers.args]
seq = "n"
[action.triggers.resolve_target]
kind = "same_id"
"#;

const RELAY: &str = r#"
[automaton]
name = "A"
states = ["Open"]
initial = "Open"

[[state]]
name = "seq"
type = "counter"
initial = 0

[[action]]
name = "Copy"
kind = "input"
from = ["Open"]
to = "Open"
params = [{ name = "seq", type = "counter" }]
effect = ["seq = params.seq"]

[[action.constraints]]
kind = "param_greater_than_field"
param = "seq"
field = "seq"

[[action.triggers]]
name = "pass"
kind = "entity"
target_entity = "B"
target_action = "Copy"
[action.triggers.args]
seq = "seq"
[action.triggers.resolve_target]
kind = "same_id"
"#;

const SINK: &str = r#"
[automaton]
name = "B"
states = ["Open"]
initial = "Open"

[[state]]
name = "seq"
type = "counter"
initial = 0

[[state]]
name = "copies"
type = "counter"
initial = 0

[[action]]
name = "Copy"
kind = "input"
from = ["Open"]
to = "Open"
params = [{ name = "seq", type = "counter" }]
effect = ["seq = params.seq", "copies += 1"]

[[action.constraints]]
kind = "param_greater_than_field"
param = "seq"
field = "seq"

[[invariant]]
name = "rule"
assert = "copies <= 1 || A[Id].status == 'Gone'"
"#;

#[test]
fn a_reaction_from_outside_the_scope_may_carry_a_newer_count() {
    // Src bumps twice: A takes 1 and then 2, and passes each on to B, which
    // takes both. The walk cannot keep A's count by how far behind the
    // newest it is, since the newest comes from outside.
    let policies = r#"permit(principal, action == Action::"Bump", resource);
permit(principal, action == Action::"Copy", resource) when { context has via && context.via != "request" };"#;
    let result = invariant(&[SOURCE, RELAY, SINK], policies);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}
