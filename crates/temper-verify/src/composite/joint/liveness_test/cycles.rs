//! "From every state the walk finds, the goal can still be reached" over a
//! found graph with cycles in it. Stateright's own `eventually` says it works
//! only on acyclic paths, and looks for the goal along a path rather than from
//! each state, so these are the cases it cannot decide.

use std::sync::atomic::Ordering;

use stateright::{Checker, Model, Property};

use super::*;

/// A record with a way round and round, `extra` deciding where else it goes.
fn looping(goal: &str, extra: &str, relies_on: &str) -> Automaton {
    parse_automaton(&format!(
        r#"
[automaton]
name = "Lap"
states = ["Start", "Far", "Done", "Back", "Away"]
initial = "Start"

[[action]]
name = "Go"
kind = "input"
from = ["Start"]
to = "Far"

[[action]]
name = "Round"
kind = "input"
from = ["Far"]
to = "Start"
{extra}
[[liveness]]
name = "ends"
eventually = "{goal}"
relies_on = [{relies_on}]
"#
    ))
    .expect("fixture parses")
}

const LEADS_OUT: &str = r#"
[[action]]
name = "Finish"
kind = "input"
from = ["Far"]
to = "Done"
"#;

/// Reaches the goal, then leaves it for a cycle with no way back.
const GOES_PAST: &str = r#"
[[action]]
name = "Finish"
kind = "input"
from = ["Far"]
to = "Done"

[[action]]
name = "Leave"
kind = "input"
from = ["Done"]
to = "Back"

[[action]]
name = "Spin"
kind = "input"
from = ["Back"]
to = "Away"

[[action]]
name = "Turn"
kind = "input"
from = ["Away"]
to = "Back"
"#;

const DONE: &str = "status == 'Done'";

fn check_on(automaton: &Automaton, threads: usize) -> JointLivenessResult {
    let (room, progress) = (
        Bytes::new(1 << 28).with_threads(threads),
        Progress::default(),
    );
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    verify_joint_liveness(&[automaton], None, bound).remove(0)
}

/// The cases this comes to rest in, on one thread and on several: the
/// verdict and how many states it walked are the same.
fn stuck_cases(automaton: &Automaton) -> Vec<String> {
    let one = check_on(automaton, 1);
    let four = check_on(automaton, 4);
    assert_eq!(one.states, four.states, "{one} / {four}");
    assert_eq!(resting(&one).len(), resting(&four).len(), "{one} / {four}");
    resting(&one)
}

#[test]
fn a_finished_walk_says_how_many_moves_it_covered() {
    let lap = looping(DONE, LEADS_OUT, r#""Go", "Finish""#);
    for threads in [1, 4] {
        let (room, progress) = (
            Bytes::new(1 << 28).with_threads(threads),
            Progress::default(),
        );
        let bound = Bound {
            room: &room,
            progress: &progress,
        };
        verify_joint_liveness(&[&lap], None, bound);
        assert_eq!(
            progress.depth.load(Ordering::Relaxed),
            2,
            "Start, Far, Done"
        );
    }
}

#[test]
fn a_cycle_that_never_meets_the_goal_never_settles() {
    let cases = stuck_cases(&looping(DONE, "", r#""Go", "Round""#));
    assert!(
        cases
            .iter()
            .any(|state| state.contains("Far") || state.contains("Start")),
        "{cases:?}"
    );
}

#[test]
fn a_cycle_with_a_way_out_to_the_goal_settles() {
    let lap = looping(DONE, LEADS_OUT, r#""Go", "Finish""#);
    for threads in [1, 4] {
        let result = check_on(&lap, threads);
        assert!(
            matches!(result.outcome, LivenessOutcome::Holds),
            "{threads} threads: {result}"
        );
    }
}

#[test]
fn meeting_the_goal_on_the_way_to_a_cycle_does_not_settle_it() {
    let cases = stuck_cases(&looping(
        DONE,
        GOES_PAST,
        r#""Go", "Finish", "Spin", "Turn""#,
    ));
    assert!(
        cases
            .iter()
            .any(|state| state.contains("Back") || state.contains("Away")),
        "{cases:?}"
    );
}

/// The two graphs above as Stateright sees them, with its own `eventually`.
#[derive(Clone)]
struct Graph {
    edges: &'static [(u8, u8)],
}

impl Model for Graph {
    type State = u8;
    type Action = u8;

    fn init_states(&self) -> Vec<u8> {
        vec![0]
    }

    fn actions(&self, state: &u8, actions: &mut Vec<u8>) {
        actions.extend(
            self.edges
                .iter()
                .filter(|(from, _)| from == state)
                .map(|(_, to)| *to),
        );
    }

    fn next_state(&self, _: &u8, action: u8) -> Option<u8> {
        Some(action)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![Property::eventually("ends", |_, state| *state == 2)]
    }
}

/// Why the walk keeps its graph and decides over it: with Stateright's
/// `eventually`, neither stuck case above is found, and a walk built on it
/// would call both of them fine. If this starts failing, Stateright has
/// learnt to see them.
#[test]
fn stateright_eventually_finds_neither_cycle() {
    for edges in [
        &[(0, 1), (1, 0)][..],
        &[(0, 1), (1, 0), (1, 2), (2, 3), (3, 4), (4, 3)][..],
    ] {
        let checker = Graph { edges }.checker().spawn_bfs().join();
        assert!(checker.discovery("ends").is_none(), "{edges:?}");
    }
}
