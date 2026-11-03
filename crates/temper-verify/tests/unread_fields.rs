//! The model holds only the counters, booleans and lists some check reads.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};
use temper_verify::check_model;
use temper_verify::composite::{Bound, Bytes, JointOutcome, Progress, verify_joint_invariants};
use temper_verify::model::build_model_from_ioa;

/// A record that is noted any number of times and then closed. `fields`
/// declares more fields, `note` adds effects to `Note`, and `rules` adds
/// `[[invariant]]`s.
fn spec(fields: &str, note: &str, rules: &str) -> String {
    format!(
        r#"
[automaton]
name = "Ticket"
states = ["Open", "Closed"]
initial = "Open"
{fields}
[[action]]
name = "Note"
kind = "input"
from = ["Open"]
effect = [{note}]

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"
{rules}
"#
    )
}

const FIELDS: &str = r#"
[[state]]
name = "notes"
type = "counter"
initial = 0

[[state]]
name = "seen"
type = "bool"
initial = false

[[state]]
name = "total"
type = "counter"
initial = 0
"#;

fn explored(source: &str) -> usize {
    check_model(&build_model_from_ioa(source, 2).unwrap()).states_explored
}

/// A counter and a boolean that a move writes and no check reads add no
/// states: no level reads their value. Holding them took a spec from
/// 137,137 states to 411,411.
#[test]
fn fields_no_check_reads_add_no_states() {
    let written = spec(FIELDS, r#""notes += 1", "seen = true""#, "");
    assert_eq!(explored(&written), explored(&spec("", "", "")));
}

/// A counter an invariant reads is still held, and the move that breaks the
/// invariant is still found.
#[test]
fn a_counter_an_invariant_reads_is_held_and_its_break_found() {
    let source = spec(
        FIELDS,
        r#""notes += 1""#,
        "[[invariant]]\nname = \"few\"\nassert = \"notes < 2\"\n",
    );
    let result = check_model(&build_model_from_ioa(&source, 2).unwrap());
    assert!(
        result
            .counterexamples
            .iter()
            .any(|c| c.property == "Invariants"),
        "{result:#?}"
    );
}

/// A counter only a guard reads is still held: `Close` needs two notes and
/// still fires after them. With a rule on it, a guard that lets a record
/// close after one note is still found.
#[test]
fn a_counter_a_guard_reads_is_held_and_its_break_found() {
    let close_after = |guard: &str| {
        spec(FIELDS, r#""notes += 1""#, "").replace(
            "to = \"Closed\"",
            &format!("to = \"Closed\"\nguard = \"{guard}\""),
        )
    };
    let only_guard = check_model(&build_model_from_ioa(&close_after("notes >= 2"), 2).unwrap());
    assert!(only_guard.all_properties_hold, "{only_guard:#?}");
    assert!(only_guard.dead_transitions.is_empty(), "{only_guard:#?}");
    let guarded = |guard: &str| {
        close_after(guard)
            + "[[invariant]]\nname = \"closed_after_two\"\nassert = \"status in ['Closed'] => notes >= 2\"\n"
    };
    let held = check_model(&build_model_from_ioa(&guarded("notes >= 2"), 2).unwrap());
    assert!(held.all_properties_hold, "{held:#?}");
    let broken = check_model(&build_model_from_ioa(&guarded("notes >= 1"), 2).unwrap());
    assert!(
        broken
            .counterexamples
            .iter()
            .any(|c| c.property == "Invariants"),
        "{broken:#?}"
    );
}

/// A counter copied into one an invariant reads is held too: its value is
/// the one the invariant sees.
#[test]
fn a_counter_copied_into_a_read_one_is_held() {
    let source = spec(
        FIELDS,
        r#""notes += 1", "total = notes""#,
        "[[invariant]]\nname = \"few\"\nassert = \"total < 2\"\n",
    );
    let result = check_model(&build_model_from_ioa(&source, 2).unwrap());
    assert!(
        result
            .counterexamples
            .iter()
            .any(|c| c.property == "Invariants"),
        "{result:#?}"
    );
}

/// A parent whose `Close` tells its child to mark itself once it has been
/// pinged. No guard or rule of the parent reads `pings`; only that trigger
/// does.
const PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Closed"]
initial = "Open"

[[state]]
name = "pings"
type = "counter"
initial = 0

[[action]]
name = "Ping"
kind = "input"
from = ["Open"]
effect = ["pings += 1"]

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action.triggers]]
name = "mark"
kind = "entity"
principal = "mark"
target_entity = "Child"
target_action = "Mark"
guard = "pings >= 1"

[action.triggers.resolve_target]
kind = "same_id"
"#;

const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Marked"]
initial = "Waiting"

[[action]]
name = "Mark"
kind = "internal"
from = ["Waiting"]
to = "Marked"

[[invariant]]
name = "marked_while_open"
assert = "status == 'Marked' => Parent[Id].status == 'Open'"
"#;

/// Anyone may ping or close a parent; only the parent's trigger marks a
/// child.
const ONLY_MARK_MARKS: &str = r#"
permit(principal, action in [Action::"Ping", Action::"Close"], resource);
permit(principal == Agent::"service:mark", action == Action::"Mark", resource is Child);
"#;

/// The joint model decides a trigger's guard on its sender's fields, so a
/// field only a trigger's guard reads is held there, and a ping then a close
/// marks the child after its parent closed. Left out, `pings` would read as
/// 0, the trigger would never fire and the break would go unseen.
#[test]
fn a_field_only_a_trigger_reads_is_held_in_the_joint_model() {
    let automatons: Vec<Automaton> = [PARENT, CHILD]
        .iter()
        .map(|source| parse_automaton(source).unwrap())
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let analysis = SendAnalysis::new(ONLY_MARK_MARKS).unwrap();
    let results = verify_joint_invariants(&refs, Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0].outcome, JointOutcome::Broken { .. }),
        "{}",
        results[0]
    );
}
