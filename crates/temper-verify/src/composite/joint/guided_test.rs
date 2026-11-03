//! Phase two: guided walks past where the breadth-first walk's room ends.

use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bound, Bytes, Progress};
use super::{
    JointInvariantResult, JointLivenessResult, JointOutcome, LivenessOutcome,
    verify_joint_invariants, verify_joint_liveness,
};

/// A record that goes S0, S1, ... S`depth` one move at a time and may then
/// fall into Trap, with `flags` booleans anyone may set and clear in any
/// status. The joint states number about (depth + 2) * 2^flags, so a small
/// room holds only the first few steps breadth first, and Trap lies past
/// them. `rule` is appended as written; `READ` stands for a clause that is
/// always true and reads every flag, so the model keeps them.
fn deep(depth: usize, flags: usize, rule: &str) -> String {
    let mut statuses: Vec<String> = (0..=depth).map(|j| format!("S{j}")).collect();
    statuses.push("Trap".into());
    let all = statuses
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut spec = format!("[automaton]\nname = 'Deep'\nstates = [{all}]\ninitial = 'S0'\n");
    for i in 0..flags {
        spec.push_str(&format!(
            "\n[[state]]\nname = 'b{i}'\ntype = 'bool'\ninitial = false\n"
        ));
    }
    for j in 0..depth {
        spec.push_str(&format!(
            "\n[[action]]\nname = 'Step{j}'\nkind = 'input'\nfrom = ['S{j}']\nto = 'S{}'\n",
            j + 1
        ));
    }
    spec.push_str(&format!(
        "\n[[action]]\nname = 'Fall'\nkind = 'input'\nfrom = ['S{depth}']\nto = 'Trap'\n"
    ));
    for i in 0..flags {
        spec.push_str(&format!(
            "\n[[action]]\nname = 'Set{i}'\nkind = 'input'\nfrom = [{all}]\neffect = ['b{i} = true']\n\
             \n[[action]]\nname = 'Clear{i}'\nkind = 'input'\nfrom = [{all}]\neffect = ['b{i} = false']\n"
        ));
    }
    let read = (0..flags)
        .map(|i| format!("(b{i} || !b{i})"))
        .collect::<Vec<_>>()
        .join(" && ");
    spec.push('\n');
    spec.push_str(&rule.replace("READ", &read));
    spec
}

/// The other record an invariant reads: nothing moves it.
const OTHER: &str =
    "[automaton]\nname = 'Other'\nstates = ['Open', 'Closed', 'Gone']\ninitial = 'Open'\n";

/// Broken once Deep falls into Trap: Other is never Closed.
const BREAKS_IN_TRAP: &str = "[[invariant]]\nname = 'rule'\nassert = \"status == 'Trap' => Other[Id].status == 'Closed' && READ\"\n";

/// Holds everywhere: Other is never Gone.
const HOLDS: &str = "[[invariant]]\nname = 'rule'\nassert = \"status == 'Trap' => Other[Id].status != 'Gone' && READ\"\n";

fn parse(sources: &[&str]) -> Vec<Automaton> {
    sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect()
}

fn invariant_in(sources: &[&str], room: usize) -> JointInvariantResult {
    let automatons = parse(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let (room, progress) = (Bytes::new(room), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&refs, None, bound);
    assert_eq!(room.held(), 0, "the walks give back what they took");
    assert_eq!(results.len(), 1);
    results.remove(0)
}

fn liveness_in(sources: &[&str], room: usize) -> JointLivenessResult {
    let automatons = parse(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let (room, progress) = (Bytes::new(room), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&refs, None, bound);
    assert_eq!(room.held(), 0, "the walks give back what they took");
    assert_eq!(results.len(), 1);
    results.remove(0)
}

/// Rooms the fixtures fill breadth first long before Trap.
const SMALL: usize = 2 << 20;
const DEPTH: usize = 24;
const FLAGS: usize = 10;

#[test]
fn a_break_past_the_breadth_first_room_is_found_with_a_trace_that_replays() {
    let deep = deep(DEPTH, FLAGS, BREAKS_IN_TRAP);
    let result = invariant_in(&[&deep, OTHER], SMALL);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("a reachable break past the room is BROKEN, got {result}");
    };
    // From a start, through every step of the chain, into Trap.
    assert!(path[0].action.is_none(), "{result}");
    assert!(path[0].state.contains("Deep=S0"), "{result}");
    assert!(path.len() > DEPTH + 1, "{result}");
    assert!(path.last().unwrap().state.contains("Deep=Trap"), "{result}");
    let steps = path
        .iter()
        .filter(|s| {
            s.action
                .as_deref()
                .is_some_and(|a| a.starts_with("Deep.Step"))
        })
        .count();
    assert_eq!(steps, DEPTH, "{result}");
    // The breadth-first walk alone would have stopped short of it.
    assert!(result.states > 0, "{result}");
}

#[test]
fn a_rule_that_holds_past_the_room_stays_not_checked_and_says_how_far_it_got() {
    let deep = deep(DEPTH, FLAGS, HOLDS);
    let result = invariant_in(&[&deep, OTHER], SMALL);
    let JointOutcome::Incomplete { reason } = &result.outcome else {
        panic!("phase two never says HOLDS, got {result}");
    };
    assert!(reason.contains("does not fit"), "{reason}");
    assert!(reason.contains("guided walks made"), "{reason}");
    assert!(reason.contains("distinct states"), "{reason}");
    assert!(reason.contains("moves from a start"), "{reason}");
}

#[test]
fn a_stuck_state_past_the_room_is_stuck_once_its_own_walk_finishes() {
    let deep = deep(
        DEPTH,
        FLAGS,
        "[[liveness]]\nname = 'rule'\neventually = \"status != 'Trap' && READ\"\n",
    );
    let result = liveness_in(&[&deep], SMALL);
    let LivenessOutcome::Stuck { cases, .. } = &result.outcome else {
        panic!("Trap has no way out, got {result}");
    };
    let path = &cases[0].path;
    assert!(path[0].action.is_none(), "{result}");
    assert!(path.len() > DEPTH + 1, "{result}");
    assert!(path.last().unwrap().state.contains("Deep=Trap"), "{result}");
}

#[test]
fn a_stuck_state_whose_own_walk_cannot_finish_stays_not_checked() {
    // Outside code is relied on to set and clear every flag while the
    // condition is false, so from Trap the walk the verdict rests on is
    // every way the flags can be: 2^12 states, more than its share of
    // steps (an eighth of 20,000).
    let many = 12;
    let flags = (0..many)
        .flat_map(|i| [format!("'Set{i}'"), format!("'Clear{i}'")])
        .collect::<Vec<_>>()
        .join(", ");
    let rule = format!(
        "[[liveness]]\nname = 'rule'\neventually = \"status != 'Trap' && READ\"\nrelies_on = [{flags}]\n"
    );
    let deep = deep(DEPTH, many, &rule);
    let result = liveness_in(&[&deep], SMALL);
    let LivenessOutcome::Incomplete { reason } = &result.outcome else {
        panic!("a stuck verdict needs its own walk to finish, got {result}");
    };
    assert!(reason.contains("could not finish"), "{reason}");
}

#[test]
fn the_same_specs_give_the_same_verdict_and_trace() {
    let deep = deep(DEPTH, FLAGS, BREAKS_IN_TRAP);
    let first = invariant_in(&[&deep, OTHER], SMALL).to_string();
    let second = invariant_in(&[&deep, OTHER], SMALL).to_string();
    assert!(first.contains("BROKEN"), "{first}");
    assert_eq!(first, second);
    let holds = deep_holds();
    assert_eq!(holds.0, holds.1);
}

fn deep_holds() -> (String, String) {
    let deep = deep(DEPTH, FLAGS, HOLDS);
    (
        invariant_in(&[&deep, OTHER], SMALL).to_string(),
        invariant_in(&[&deep, OTHER], SMALL).to_string(),
    )
}
