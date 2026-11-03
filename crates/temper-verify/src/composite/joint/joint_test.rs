use std::collections::BTreeMap;

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::*;

/// A parent whose `value` its child copies through a reaction, and which a
/// child may finish only while the parent is Closed. `reopens` lets a Closed
/// parent open again.
fn parent(reopens: bool) -> String {
    let reopen = if reopens {
        r#"
[[action]]
name = "Reopen"
kind = "input"
from = ["Closed"]
to = "Open"
"#
    } else {
        ""
    };
    format!(
        r#"
[automaton]
name = "Parent"
states = ["Open", "Closed"]
initial = "Open"

[[state]]
name = "value"
type = "string"
initial = ""

[[action]]
name = "Set"
kind = "input"
from = ["Open"]
to = "Open"
params = [{{ name = "value", type = "string" }}]

[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "Child"
target_action = "Copy"

[action.triggers.args]
value = "value"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"
{reopen}
"#
    )
}

/// The child: `Copy` takes any value anyone sends; `Finish` needs its parent
/// Closed.
const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Done", "Finished"]
initial = "Waiting"

[[state]]
name = "value"
type = "string"
initial = ""

[[action]]
name = "Copy"
kind = "internal"
from = ["Waiting", "Done"]
to = "Done"
params = [{ name = "value", type = "string" }]

[[action]]
name = "Finish"
kind = "input"
from = ["Waiting"]
to = "Finished"
guard = "Parent[Id].status == 'Closed'"
"#;

fn child_with(invariant: &str) -> String {
    format!("{CHILD}\n[[invariant]]\nname = \"rule\"\nassert = \"{invariant}\"\n")
}

fn parse(sources: &[&str]) -> Vec<Automaton> {
    sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect()
}

fn check(sources: &[&str]) -> JointInvariantResult {
    check_under(sources, None)
}

/// The one result for `sources`, with `policies` (Cedar) deciding each move
/// as its sender.
fn check_under(sources: &[&str], policies: Option<&str>) -> JointInvariantResult {
    let automatons = parse(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = policies.map(|text| SendAnalysis::new(text).expect("fixture policies parse"));
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&refs, analysis.as_ref(), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

fn trace(entity: &str, action: &str, params: &[(&str, &str)]) -> TraceMove {
    TraceMove {
        entity: entity.to_string(),
        action: action.to_string(),
        params: params
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn ids(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(entity, id)| (entity.to_string(), id.to_string()))
        .collect()
}

/// Whether nothing breaks `result`'s rule and no settled state meets
/// `premise`: how a fixture shows a move never lands.
fn never_meets(result: &JointInvariantResult, premise: &str) -> bool {
    matches!(&result.outcome, JointOutcome::Vacuous { premise: met } if met == premise)
}

#[test]
fn a_copy_anyone_may_write_breaks_the_invariant_with_the_move_that_wrote_it() {
    let child = child_with("status == 'Done' => Parent[Id].value == value");
    let parent = parent(false);
    let result = check(&[&parent, &child]);
    assert_eq!(
        (result.entity.as_str(), result.invariant.as_str()),
        ("Child", "rule")
    );
    assert_eq!(result.scope, ["Child", "Parent"]);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected a broken invariant, got {result}");
    };
    assert_eq!(
        path.len(),
        2,
        "start, then the move that breaks it: {result}"
    );
    assert!(
        path[1]
            .action
            .as_deref()
            .is_some_and(|a| a.starts_with("Child.Copy")),
        "{result}"
    );
}

/// A parent set once, whose `tell` runs as its own service, and Cedar that
/// lets only that service send the child's Copy.
fn parent_told_once() -> String {
    parent(false)
        .replace(
            "from = [\"Open\"]\nto = \"Open\"\nparams",
            "from = [\"Open\"]\nto = \"Closed\"\nparams",
        )
        .replace(
            "name = \"tell\"\nkind = \"entity\"",
            "name = \"tell\"\nkind = \"entity\"\nprincipal = \"tell\"",
        )
}

const ONLY_TELL_COPIES: &str = r#"
permit(principal, action in [Action::"Set", Action::"Close", Action::"Finish"], resource);
permit(principal == Agent::"service:tell", action == Action::"Copy", resource is Child);
"#;

#[test]
fn a_copy_only_the_parents_service_may_send_holds_under_its_cedar() {
    let child = child_with("status == 'Done' => Parent[Id].value == value");
    let parent = parent_told_once();
    assert!(
        !check(&[&parent, &child]).holds(),
        "without Cedar anyone may send Copy"
    );
    let result = check_under(&[&parent, &child], Some(ONLY_TELL_COPIES));
    assert!(result.holds(), "{result}");
}

#[test]
fn an_action_a_liveness_relies_on_is_sent_whatever_cedar_says() {
    let child = format!(
        "{}\n[[liveness]]\nname = \"moves_on\"\neventually = \"status != 'Waiting'\"\nrelies_on = [\"Copy\"]\n",
        child_with("status == 'Done' => Parent[Id].value == value")
    );
    let parent = parent_told_once();
    let result = check_under(&[&parent, &child], Some(ONLY_TELL_COPIES));
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected a broken invariant, got {result}");
    };
    assert!(
        path.iter()
            .filter_map(|step| step.action.as_deref())
            .any(|a| a.starts_with("Child.Copy")),
        "{result}"
    );
}

#[test]
fn a_guard_on_a_related_status_that_cannot_change_back_holds() {
    let child = child_with("status == 'Finished' => Parent[Id].status == 'Closed'");
    let parent = parent(false);
    let result = check(&[&parent, &child]);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_related_status_read_on_the_dispatch_path_can_change_before_the_write() {
    let child = child_with("status == 'Finished' => Parent[Id].status == 'Closed'");
    let parent = parent(true);
    let result = check(&[&parent, &child]);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected a broken invariant, got {result}");
    };
    let moves: Vec<&str> = path.iter().filter_map(|s| s.action.as_deref()).collect();
    let sent = moves
        .iter()
        .position(|m| m.starts_with("Child.Finish") && m.ends_with("sent"));
    let reopened = moves.iter().position(|m| m.starts_with("Parent.Reopen"));
    let arrived = moves
        .iter()
        .position(|m| m.starts_with("Child.Finish") && m.contains("arrives"));
    assert!(
        matches!((sent, reopened, arrived), (Some(s), Some(r), Some(a)) if s < r && r < a),
        "Finish is sent while the parent is Closed, the parent reopens, then Finish lands: {result}"
    );
}

#[test]
fn an_invariant_naming_an_undeclared_type_is_not_checked_and_says_why() {
    let child = child_with("status == 'Done' => Parent[Id].value == value");
    let result = check(&[&child]);
    let JointOutcome::Incomplete { reason } = &result.outcome else {
        panic!("expected not checked, got {result}");
    };
    assert!(reason.contains("Parent"), "{reason}");
    assert!(!result.holds());
}

#[test]
fn specs_without_cross_entity_invariants_have_nothing_to_check() {
    let parent = parent(false);
    let automatons = parse(&[&parent, CHILD]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    assert!(verify_joint_invariants(&refs, None, bound).is_empty());
}

#[test]
fn a_replayed_reaction_is_in_flight_until_the_trace_takes_it() {
    let child = child_with("status == 'Done' => Parent[Id].value == value");
    let parent = parent(false);
    let automatons = parse(&[&parent, &child]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let same = ids(&[("Parent", "p"), ("Child", "p")]);

    let set = replay_joint_invariants(
        &refs,
        "Child",
        &same,
        &[trace("Parent", "Set", &[("value", "A")])],
    )
    .expect("Set is allowed");
    let last = &set.steps.last().unwrap().state;
    assert!(last.contains("in flight: Child.Copy"), "{last}");

    let copied = replay_joint_invariants(
        &refs,
        "Child",
        &same,
        &[
            trace("Parent", "Set", &[("value", "A")]),
            trace("Child", "Copy", &[("value", "A")]),
        ],
    )
    .expect("the trace takes the reaction");
    assert!(copied.broken.is_empty());
    let last = &copied.steps.last().unwrap().state;
    assert!(
        last.contains("Child=Done") && !last.contains("in flight"),
        "{last}"
    );

    let forged = replay_joint_invariants(
        &refs,
        "Child",
        &same,
        &[trace("Child", "Copy", &[("value", "B")])],
    )
    .expect("anyone may send Copy");
    assert_eq!(forged.broken, ["rule"]);
}

#[test]
fn a_same_id_reaction_reaches_no_record_with_another_id() {
    let child = child_with("status == 'Done' => Parent[Id].value == value");
    let parent = parent(false);
    let automatons = parse(&[&parent, &child]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let apart = ids(&[("Parent", "p"), ("Child", "c")]);
    let set = replay_joint_invariants(
        &refs,
        "Child",
        &apart,
        &[trace("Parent", "Set", &[("value", "A")])],
    )
    .expect("Set is allowed");
    let last = &set.steps.last().unwrap().state;
    assert!(
        !last.contains("in flight"),
        "the reaction found no child: {last}"
    );
}

#[test]
fn a_value_only_the_invariant_names_is_one_a_request_may_send() {
    let child = child_with("Parent[Id].value == 'secret' => status == 'Waiting'");
    let parent = parent(false);
    let result = check(&[&parent, &child]);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected a broken invariant, got {result}");
    };
    assert!(
        path.iter()
            .filter_map(|s| s.action.as_deref())
            .any(|m| m.starts_with("Parent.Set") && m.contains("'secret'")),
        "{result}"
    );
}

/// A parent that tells its child each value it is set to, then seals the
/// child with the value it ended on. The child takes the seal only while it
/// holds that value: `sealed_with` must equal `value`, and the child has no
/// field of that name.
const SEALING_PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Closed"]
initial = "Open"

[[state]]
name = "value"
type = "string"
initial = ""

[[action]]
name = "Set"
kind = "input"
from = ["Open"]
to = "Open"
params = [{ name = "value", type = "string" }]

[[action.triggers]]
name = "tell"
kind = "entity"
principal = "tell"
target_entity = "Child"
target_action = "Copy"

[action.triggers.args]
value = "value"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action.triggers]]
name = "seal"
kind = "entity"
principal = "tell"
target_entity = "Child"
target_action = "Seal"

[action.triggers.args]
sealed_with = "value"

[action.triggers.resolve_target]
kind = "same_id"
"#;

const SEALED_CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Done", "Sealed"]
initial = "Waiting"

[[state]]
name = "value"
type = "string"
initial = ""

[[action]]
name = "Copy"
kind = "internal"
from = ["Waiting", "Done"]
to = "Done"
params = [{ name = "value", type = "string" }]

[[action]]
name = "Seal"
kind = "internal"
from = ["Done"]
to = "Sealed"
params = [{ name = "sealed_with", type = "string" }]

[[action.constraints]]
kind = "param_equals_field"
param = "sealed_with"
field = "value"

[[invariant]]
name = "sealed_with_the_parents_value"
assert = "status == 'Sealed' => Parent[Id].value == value"
"#;

const ONLY_TELL_REACHES_THE_CHILD: &str = r#"
permit(principal, action in [Action::"Set", Action::"Close"], resource is Parent);
permit(principal == Agent::"service:tell", action in [Action::"Copy", Action::"Seal"], resource is Child);
"#;

#[test]
fn a_constraint_on_a_parameter_that_writes_no_field_is_decided() {
    // Set A, Set B, Close: Copy(A), Copy(B) and Seal(B) are in flight. If
    // Copy(A) lands last of the copies, the child holds A and refuses
    // Seal(B). A model that cannot read `sealed_with` lets the seal land
    // and reports the child sealed with A while the parent ended on B.
    let result = check_under(
        &[SEALING_PARENT, SEALED_CHILD],
        Some(ONLY_TELL_REACHES_THE_CHILD),
    );
    assert!(result.holds(), "{result}");
}

#[test]
fn a_seal_sent_without_its_value_is_refused() {
    // The runtime refuses a constrained parameter nobody passed, so a seal
    // that does not carry `sealed_with` never lands. A seal comes only once
    // the parent is Closed, so this rule breaks if one ever lands, and no
    // settled state has the child Sealed if none does.
    let parent = SEALING_PARENT.replace("sealed_with = \"value\"", "");
    let child = SEALED_CHILD.replace(
        "assert = \"status == 'Sealed' => Parent[Id].value == value\"",
        "assert = \"status == 'Sealed' => Parent[Id].value == value && Parent[Id].status == 'Open'\"",
    );
    let result = check_under(&[&parent, &child], Some(ONLY_TELL_REACHES_THE_CHILD));
    assert!(never_meets(&result, "status == 'Sealed'"), "{result}");
}

#[path = "equalities_test.rs"]
mod equalities;

#[test]
fn a_walk_its_room_cannot_hold_is_not_checked_and_gives_its_memory_back() {
    let child = child_with("status == 'Finished' => Parent[Id].status == 'Closed'");
    let parent = parent(false);
    let automatons = parse(&[&parent, &child]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let (room, progress) = (Bytes::new(4 << 10), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let results = verify_joint_invariants(&refs, None, bound);
    assert_eq!(results.len(), 1);
    let JointOutcome::Incomplete { reason } = &results[0].outcome else {
        panic!("expected not checked, got {}", results[0]);
    };
    assert!(reason.contains("does not fit"), "{reason}");
    assert_eq!(room.held(), 0, "the walk gives back what it took");
}

#[test]
fn a_sequence_copied_by_effect_keeps_its_actual_value() {
    let parent = r#"
[automaton]
name = "Parent"
states = ["Zero", "One", "Two", "Copied"]
initial = "Zero"
[[state]]
name = "sequence"
type = "counter"
initial = 0
[[state]]
name = "snapshot"
type = "counter"
initial = 0
[[action]]
name = "First"
kind = "input"
from = ["Zero"]
to = "One"
effect = ["sequence += 1"]
[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "Child"
target_action = "Copy"
principal = "counter"
[action.triggers.args]
sequence = "sequence"
[action.triggers.resolve_target]
kind = "same_id"
[[action]]
name = "Second"
kind = "input"
from = ["One"]
to = "Two"
effect = ["sequence += 1"]
[[action]]
name = "Snapshot"
kind = "input"
from = ["Two"]
to = "Copied"
effect = ["snapshot = sequence"]
"#;
    let child = r#"
[automaton]
name = "Child"
states = ["Waiting", "Done"]
initial = "Waiting"
[[state]]
name = "sequence"
type = "counter"
initial = 0
[[action]]
name = "Copy"
kind = "input"
from = ["Waiting", "Done"]
to = "Done"
params = [{ name = "sequence", type = "counter" }]
effect = ["sequence = params.sequence"]
[[action.constraints]]
kind = "param_greater_than_field"
param = "sequence"
field = "sequence"
[[invariant]]
name = "snapshot_bound"
assert = "status == 'Done' => Parent[Id].snapshot <= 1"
"#;
    let policies = r#"
permit(principal, action in [Action::"First", Action::"Second", Action::"Snapshot"], resource);
permit(principal, action == Action::"Copy", resource is Child) when {
    principal == Agent::"service:counter" && context has triggerSourceEntityId &&
    resource has id && context.triggerSourceEntityId == resource.id
};
"#;
    let result = check_under(&[parent, child], Some(policies));
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("the copied sequence must violate the invariant: {result}");
    };
    assert!(
        path.iter().any(|step| step
            .action
            .as_deref()
            .is_some_and(|a| a.starts_with("Parent.Snapshot"))),
        "{result}"
    );
    assert!(
        path.last().unwrap().state.contains("snapshot=2"),
        "{result}"
    );
}

#[test]
fn unknown_related_reads_cannot_be_proven() {
    for assertion in [
        "Parent[Id].score > 0",
        "Parent[Id].absent == 'x'",
        "Parent[absent].status == 'Closed'",
    ] {
        let parent = "[automaton]\nname = 'Parent'\nstates = ['Open']\ninitial = 'Open'\n[[state]]\nname = 'score'\ntype = 'int'\ninitial = 1\n";
        let child = child_with(assertion);
        let result = check(&[parent, &child]);
        assert!(
            matches!(result.outcome, JointOutcome::Incomplete { .. }),
            "{result}"
        );
    }
}

#[test]
fn a_literal_in_a_related_read_is_explored_by_the_parent_setter() {
    let child = child_with("Parent[Id].value != 'bad'");
    let result = check(&[&parent(false), &child]);
    let JointOutcome::Broken { path } = result.outcome else {
        panic!("{result}");
    };
    assert!(path.iter().any(|s| s.state.contains("value='bad'")));
}

#[test]
fn initial_partitions_stop_within_a_tiny_room() {
    let sources: Vec<String> = (0..13)
        .map(|i| format!("[automaton]\nname = 'T{i}'\nstates = ['Ready']\ninitial = 'Ready'\n"))
        .collect();
    let assertion = (1..13)
        .map(|i| format!("T{i}[Id].status != 'Never'"))
        .collect::<Vec<_>>()
        .join(" && ");
    let mut sources = sources;
    sources[0].push_str(&format!(
        "\n[[invariant]]\nname = 'rule'\nassert = \"{assertion}\"\n"
    ));
    let automatons = parse(&sources.iter().map(String::as_str).collect::<Vec<_>>());
    // 13 records start in 27,644,437 ways their ids can be equal: the
    // starting states take their memory from the room too.
    let (room, progress) = (Bytes::new(64 << 10), Progress::default());
    let results = verify_joint_invariants(
        &automatons.iter().collect::<Vec<_>>(),
        None,
        Bound {
            room: &room,
            progress: &progress,
        },
    );
    assert_eq!(results.len(), 1);
    let JointOutcome::Incomplete { reason } = &results[0].outcome else {
        panic!("expected not checked, got {}", results[0]);
    };
    assert!(reason.contains("does not fit"), "{reason}");
    // Every start takes at least 16 bytes of the room, so the walk stops
    // after a few thousand of them at most.
    assert!(results[0].states <= (64 << 10) / 16 + 1, "{}", results[0]);
    assert_eq!(room.held(), 0, "the walk gives back what it took");
}

/// A record whose one move sets off `reactions` reactions, each with a
/// guard on another record that the model cannot decide, so the move goes
/// two ways for each: what it builds doubles with every reaction, while the
/// states it leads to are all the same one.
fn many_undecided(reactions: usize) -> Vec<String> {
    let mut many = String::from(
        "[automaton]\nname = 'Many'\nstates = ['Start', 'Done']\ninitial = 'Start'\n\n\
         [[action]]\nname = 'Go'\nkind = 'input'\nfrom = ['Start', 'Done']\nto = 'Done'\n",
    );
    for i in 0..reactions {
        many.push_str(&format!(
            "\n[[action.triggers]]\nname = 'again{i}'\nkind = 'entity'\n\
             target_entity = 'Many'\ntarget_action = 'Go'\n\
             guard = \"Other[Id].status == 'Open'\"\n\
             [action.triggers.resolve_target]\nkind = 'same_id'\n"
        ));
    }
    many.push_str(
        "\n[[invariant]]\nname = 'rule'\n\
         assert = \"status == 'Start' || status == 'Done' || Other[Id].status == 'Open'\"\n",
    );
    let other = "[automaton]\nname = 'Other'\nstates = ['Open']\ninitial = 'Open'\n\n\
                 [[action]]\nname = 'Stay'\nkind = 'input'\nfrom = ['Open']\nto = 'Open'\n";
    vec![many, other.to_string()]
}

fn check_in(sources: &[String], room: &Bytes) -> JointInvariantResult {
    let automatons = parse(&sources.iter().map(String::as_str).collect::<Vec<_>>());
    let progress = Progress::default();
    let bound = Bound {
        room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&automatons.iter().collect::<Vec<_>>(), None, bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn what_one_move_builds_counts_against_the_room() {
    // 2^12 ways for one move, a few KB each: tens of MB built before the
    // walk keeps the one state they all lead to.
    let sources = many_undecided(12);
    let room = Bytes::new(1 << 20);
    let result = check_in(&sources, &room);
    let JointOutcome::Incomplete { reason } = &result.outcome else {
        panic!("a move that builds more than the room holds is not checked, got {result}");
    };
    assert!(reason.contains("does not fit"), "{reason}");
    assert_eq!(room.held(), 0, "the walk gives back what it took");

    // With room for what the move builds, the rule is checked and holds.
    let room = Bytes::new(1 << 30);
    let result = check_in(&sources, &room);
    assert!(matches!(result.outcome, JointOutcome::Holds), "{result}");
    assert_eq!(room.held(), 0, "the walk gives back what it took");
}

#[test]
fn replay_distinguishes_two_new_values_in_one_request() {
    let pair = r#"
[automaton]
name = "Pair"
states = ["New", "Done"]
initial = "New"
[[state]]
name = "a"
type = "string"
initial = ""
[[state]]
name = "b"
type = "string"
initial = ""
[[action]]
name = "Write"
kind = "input"
from = ["New"]
to = "Done"
params = [{ name = "a", type = "string" }, { name = "b", type = "string" }]
[[invariant]]
name = "equal"
assert = "Parent[Id].status == 'Open' => a == b"
"#;
    let parent = "[automaton]\nname = 'Parent'\nstates = ['Open']\ninitial = 'Open'\n";
    let automatons = parse(&[pair, parent]);
    let refs: Vec<_> = automatons.iter().collect();
    let same = ids(&[("Parent", "p"), ("Pair", "p")]);
    for (b, broken) in [("X", false), ("Y", true)] {
        let replay = replay_joint_invariants(
            &refs,
            "Pair",
            &same,
            &[trace("Pair", "Write", &[("a", "X"), ("b", b)])],
        )
        .unwrap();
        assert_eq!(!replay.broken.is_empty(), broken);
    }
}

#[test]
fn concrete_reference_ids_can_find_an_initially_closed_parent() {
    let parent = "[automaton]\nname = 'Parent'\nstates = ['Closed']\ninitial = 'Closed'\n";
    for id in ["550e8400-e29b-41d4-a716-446655440000", "#2"] {
        let child = format!(
            r#"
[automaton]
name = "Child"
states = ["Ready"]
initial = "Ready"
[[state]]
name = "parent_id"
type = "string"
initial = "{id}"
[[invariant]]
name = "rule"
assert = "Parent[parent_id].status != 'Closed'"
"#
        );
        let result = check(&[parent, &child]);
        let JointOutcome::Broken { path } = &result.outcome else {
            panic!("expected the concrete reference to find Parent: {result}");
        };
        assert_eq!(path.len(), 1, "the rule already fails initially: {result}");
    }
}

#[test]
fn concrete_ids_in_assertions_are_explored_without_string_fields() {
    let parent = "[automaton]\nname = 'Parent'\nstates = ['Closed']\ninitial = 'Closed'\n";
    let child = child_with("Parent[Id].id != 'concrete'");
    let result = check(&[parent, &child]);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

#[test]
fn empty_references_never_find_a_record() {
    let parent = "[automaton]\nname = 'Parent'\nstates = ['Closed']\ninitial = 'Closed'\n";
    let child = "[automaton]\nname = 'Child'\nstates = ['Ready']\ninitial = 'Ready'\n[[state]]\nname = 'parent_id'\ntype = 'string'\ninitial = ''\n[[invariant]]\nname = 'rule'\nassert = \"Parent[parent_id].status != 'Closed'\"\n";
    let result = check(&[parent, child]);
    assert!(matches!(result.outcome, JointOutcome::Holds), "{result}");
}

/// The digest of each joint proof of `sources`, with `policies` (Cedar)
/// deciding each move as its sender.
fn digests(sources: &[&str], policies: Option<&str>) -> Vec<String> {
    let automatons = parse(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = policies.map(|text| SendAnalysis::new(text).expect("fixture policies parse"));
    let room = Bytes::new(1 << 30);
    joint_proofs(&refs, analysis.as_ref(), &room)
        .iter()
        .map(JointProof::digest)
        .collect()
}

#[test]
fn a_proof_keeps_its_digest_through_a_change_its_cut_leaves_out() {
    let child = child_with("status == 'Finished' => Parent[Id].status == 'Closed'");
    let parent = parent(false);
    let before = digests(&[&parent, &child], None);
    assert_eq!(before.len(), 1);
    assert_eq!(
        before,
        digests(&[&parent, &child], None),
        "the same specs, the same digest"
    );
    // A counter the rule never reads, counted by an action the cut keeps.
    let counted = parent
        .replace(
            "[[action]]\nname = \"Close\"",
            "[[state]]\nname = \"closings\"\ntype = \"counter\"\ninitial = 0\n\n[[action]]\nname = \"Close\"",
        )
        .replace("to = \"Closed\"\n", "to = \"Closed\"\neffect = [\"closings += 1\"]\n");
    assert!(counted.contains("closings += 1"));
    assert_eq!(before, digests(&[&counted, &child], None));
    // Cedar about an action no type in scope has.
    let more = format!(
        "{ONLY_TELL_COPIES}\npermit(principal, action == Action::\"Elsewhere\", resource);\n"
    );
    assert_eq!(
        digests(&[&parent, &child], Some(ONLY_TELL_COPIES)),
        digests(&[&parent, &child], Some(&more))
    );
}

#[test]
fn a_proof_gets_a_new_digest_from_a_change_its_walk_reads() {
    let child = child_with("status == 'Finished' => Parent[Id].status == 'Closed'");
    let before = digests(&[&parent(false), &child], None);
    assert_ne!(
        before,
        digests(&[&parent(true), &child], None),
        "a parent that reopens"
    );
    let child = child_with("status == 'Done' => Parent[Id].value == value");
    let parent = parent_told_once();
    assert_ne!(
        digests(&[&parent, &child], None),
        digests(&[&parent, &child], Some(ONLY_TELL_COPIES)),
        "Cedar that decides who may send Copy"
    );
}
