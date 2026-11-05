//! A reaction from a type outside a rule's scope into a type in it: the
//! walk holds no record of its source, so it sends it as a move from
//! outside, whenever its target's guard may let it.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

fn parsed(sources: &[&str]) -> Vec<Automaton> {
    sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect()
}

/// Run `check` in a room no fixture here comes near filling.
fn bounded<T>(check: impl FnOnce(Bound<'_>) -> T) -> T {
    bounded_in(1 << 30, check)
}

fn bounded_in<T>(bytes: usize, check: impl FnOnce(Bound<'_>) -> T) -> T {
    let (room, progress) = (Bytes::new(bytes), Progress::default());
    check(Bound {
        room: &room,
        progress: &progress,
    })
}

fn invariant(sources: &[&str], policies: &str) -> JointInvariantResult {
    invariant_in(1 << 30, sources, policies)
}

fn invariant_in(bytes: usize, sources: &[&str], policies: &str) -> JointInvariantResult {
    let automatons = parsed(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let mut results = bounded_in(bytes, |bound| {
        verify_joint_invariants(&refs, Some(&analysis), bound)
    });
    assert_eq!(results.len(), 1);
    results.remove(0)
}

fn liveness(sources: &[&str], policies: &str) -> JointLivenessResult {
    let automatons = parsed(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let mut results = bounded(|bound| verify_joint_liveness(&refs, Some(&analysis), bound));
    assert_eq!(results.len(), 1);
    results.remove(0)
}

/// A record whose `Done` only a reaction from `Push` sends; `Done` may set
/// off `Box.Fill`. `rule` is appended as written.
fn ask(done_fills: bool, rule: &str) -> String {
    let fill = if done_fills {
        r#"
[[action.triggers]]
name = "fill"
kind = "entity"
target_entity = "Box"
target_action = "Fill"

[action.triggers.resolve_target]
kind = "same_id"
"#
    } else {
        ""
    };
    format!(
        r#"
[automaton]
name = "Ask"
states = ["Idle", "Asked", "Done"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Asked"

[[action]]
name = "Done"
kind = "input"
from = ["Asked"]
to = "Done"
{fill}
{rule}
"#
    )
}

/// A box only a reaction fills.
const BOX: &str = r#"
[automaton]
name = "Box"
states = ["Empty", "Full"]
initial = "Empty"

[[action]]
name = "Fill"
kind = "input"
from = ["Empty"]
to = "Full"
"#;

/// Its `Poke` takes `Ask.Done` on the record with its id, as the service
/// `pusher` when `principal` says so.
fn push(principal: bool) -> String {
    let principal = if principal {
        "principal = \"pusher\"\n"
    } else {
        ""
    };
    format!(
        r#"
[automaton]
name = "Push"
states = ["Ready", "Pushed"]
initial = "Ready"

[[action]]
name = "Poke"
kind = "input"
from = ["Ready"]
to = "Pushed"

[[action.triggers]]
name = "done"
kind = "entity"
target_entity = "Ask"
target_action = "Done"
{principal}
[action.triggers.resolve_target]
kind = "same_id"
"#
    )
}

/// Outside callers may start an ask and poke; nobody outside sends `Done`.
const POLICIES: &str =
    r#"permit(principal, action in [Action::"Start", Action::"Poke"], resource);"#;

/// As [`POLICIES`], and a member's reaction may send `Done`: Cedar may let
/// Push's reaction through or not, as its sender is a member or not.
const MEMBERS_DONE: &str = r#"
permit(principal, action in [Action::"Start", Action::"Poke"], resource);
permit(principal, action == Action::"Done", resource)
when { principal has role && principal.role == "member" && context has via && context.via != "request" };
"#;

#[test]
fn an_invariant_a_reaction_from_outside_the_scope_breaks_is_broken() {
    // Push's reaction takes Done while the box is still empty. The rule
    // names Ask and Box, not Push.
    let rule = "[[invariant]]\nname = \"rule\"\nassert = \"status == 'Done' => Box[Id].status == 'Full'\"\n";
    // The walk sends that reaction with any sender, so it walks once more
    // with Push in the scope, where sure moves alone break the rule. Cedar
    // must let the reaction through: under `POLICIES` alone the runtime
    // refuses it, and the rule holds.
    let policies = format!(
        "{POLICIES}\npermit(principal, action == Action::\"Done\", resource) when {{ context has via && context.via == \"reaction\" }};"
    );
    let result = invariant(&[&ask(false, rule), BOX, &push(false)], &policies);
    let text = result.to_string();
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected broken, got {text}");
    };
    assert!(
        path.iter().any(|step| step
            .action
            .as_deref()
            .is_some_and(|a| a.starts_with("Push.Poke"))),
        "{text}"
    );
}

#[test]
fn an_invariant_that_holds_through_a_reaction_from_outside_the_scope_holds() {
    let rule = "[[invariant]]\nname = \"rule\"\nassert = \"status == 'Done' => Box[Id].status != 'Full'\"\n";
    let result = invariant(&[&ask(false, rule), BOX, &push(false)], MEMBERS_DONE);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_reaction_set_off_from_outside_the_scope_runs_as_a_sender_the_walk_does_not_know() {
    // Push's reaction runs as the service pusher, and the Fill that Done
    // sets off inherits it. Only pusher may fill a box, and no type in the
    // rule's scope names pusher: Cedar lets the Fill through in the
    // runtime, so the box can be full while the ask is Done.
    let rule = "[[invariant]]\nname = \"rule\"\nassert = \"status == 'Done' => Box[Id].status != 'Full'\"\n";
    let policies = format!(
        "{POLICIES}\npermit(principal == Agent::\"service:pusher\", action in [Action::\"Done\", Action::\"Fill\"], resource);"
    );
    let result = invariant(&[&ask(true, rule), BOX, &push(true)], &policies);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{text}"
    );
    assert!(text.contains("Box.Fill"), "{text}");
}

/// `A.Hit`, which only `C`'s reaction sends, and only once `B` has been
/// marked, marks `B` again. `B.Mark` pings `C`, whose `Ping` hits `A`: when
/// the first mark comes from outside, the chain that hits `A` already holds
/// `B.Mark`, and the runtime stops the second one.
const HIT: &str = r#"
[automaton]
name = "A"
states = ["Idle", "Hit"]
initial = "Idle"

[[action]]
name = "Hit"
kind = "input"
from = ["Idle"]
to = "Hit"
guard = "B[Id].status == 'Marked'"

[[action.triggers]]
name = "mark"
kind = "entity"
target_entity = "B"
target_action = "Mark"

[action.triggers.resolve_target]
kind = "same_id"

[[invariant]]
name = "rule"
assert = "status == 'Hit' => B[Id].marks >= 2"
"#;

const MARK: &str = r#"
[automaton]
name = "B"
states = ["Open", "Marked"]
initial = "Open"

[[state]]
name = "marks"
type = "counter"
initial = 0

[[action]]
name = "Mark"
kind = "input"
from = ["Open", "Marked"]
to = "Marked"
effect = ["marks += 1"]

[[action.triggers]]
name = "ping"
kind = "entity"
target_entity = "C"
target_action = "Ping"

[action.triggers.resolve_target]
kind = "same_id"
"#;

const PING: &str = r#"
[automaton]
name = "C"
states = ["Open"]
initial = "Open"

[[action]]
name = "Ping"
kind = "input"
from = ["Open"]

[[action.triggers]]
name = "hit"
kind = "entity"
target_entity = "A"
target_action = "Hit"

[action.triggers.resolve_target]
kind = "same_id"
"#;

#[test]
fn a_reaction_from_outside_the_scope_may_arrive_in_a_chain_that_already_holds_what_it_sets_off() {
    // Mark from outside, then its chain through C hits A, whose own Mark is
    // stopped as a repeat: A is Hit with B marked once.
    let policies = r#"permit(principal, action == Action::"Mark", resource);
permit(principal, action in [Action::"Ping", Action::"Hit", Action::"Mark"], resource) when { context has via && context.via != "request" };"#;
    let result = invariant(&[HIT, MARK, PING], policies);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{text}"
    );
}

#[test]
fn a_stuck_state_only_a_reaction_from_outside_the_scope_reaches_never_settles_if_it_comes() {
    // The walk takes Push's reaction as a move from outside, with a sender
    // it made up, and nothing leads on from Done. The verdict rests on it.
    let rule =
        "[[liveness]]\nname = \"rule\"\neventually = \"status == 'Done' => status == 'Idle'\"\n";
    let result = liveness(&[&ask(false, rule), &push(false)], MEMBERS_DONE);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{text}"
    );
    assert!(
        text.contains("Ask.Done -> Done (a reaction from Push)"),
        "{text}"
    );
}

#[test]
fn a_stuck_state_only_a_reaction_from_outside_the_scope_may_leave_is_not_checked() {
    // Asked is left only by Push's reaction, which the walk cannot tell
    // comes: it is neither stuck nor known to settle.
    let rule = "[[liveness]]\nname = \"rule\"\neventually = \"status != 'Asked'\"\n";
    let result = liveness(&[&ask(false, rule), &push(false)], MEMBERS_DONE);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Incomplete { .. }),
        "{text}"
    );
    assert!(
        text.contains("Ask.Done (from Asked) by a reaction from Push"),
        "{text}"
    );
}

#[test]
fn a_stuck_state_only_a_reaction_cedar_refuses_everyone_may_leave_never_settles() {
    // Nobody may send Done, so Push's reaction, whoever pokes, is refused:
    // Asked is never left.
    let rule = "[[liveness]]\nname = \"rule\"\neventually = \"status != 'Asked'\"\n";
    let result = liveness(&[&ask(false, rule), &push(false)], POLICIES);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{text}"
    );
}

/// `A.Tick`, which only reactions send, sets off `A.Tick` again on the same
/// record; `Poke`, outside the scope, sends the first.
const TICK: &str = r#"
[automaton]
name = "A"
states = ["Idle", "Ticked"]
initial = "Idle"

[[action]]
name = "Tick"
kind = "internal"
from = ["Idle", "Ticked"]
to = "Ticked"

[[action.triggers]]
name = "again"
kind = "entity"
target_entity = "A"
target_action = "Tick"

[action.triggers.resolve_target]
kind = "same_id"

[[invariant]]
name = "rule"
assert = "status == 'Ticked' => Box[Id].status != 'Full'"
"#;

const POKE: &str = r#"
[automaton]
name = "P"
states = ["Ready"]
initial = "Ready"

[[action]]
name = "Poke"
kind = "input"
from = ["Ready"]
to = "Ready"

[[action.triggers]]
name = "tick"
kind = "entity"
target_entity = "A"
target_action = "Tick"

[action.triggers.resolve_target]
kind = "same_id"
"#;

#[test]
fn a_reaction_from_outside_the_scope_that_sets_itself_off_again_is_stopped() {
    // Whether Poke's chain made A's record is unknown, but the second Tick
    // carries the same origin as the first, so the runtime stops it and the
    // walk ends, well within a small room.
    let policies = r#"permit(principal, action == Action::"Poke", resource);
permit(principal, action == Action::"Tick", resource) when { context has via && context.via != "request" };"#;
    let result = invariant_in(16 << 20, &[TICK, BOX, POKE], policies);
    assert!(result.holds(), "{result}");
}
