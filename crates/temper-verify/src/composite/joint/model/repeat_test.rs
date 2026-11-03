//! Regression cases carried from the former synchronous composite model.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Automaton, parse_automaton};

use super::super::super::JointState;
use super::super::super::pack::Packer;
use super::super::super::senders::OUTSIDE;
use super::super::JointModel;
use crate::model::TemperModelAction;

fn whole(automatons: &[&Automaton]) -> JointModel {
    let by_name = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let scope = automatons
        .iter()
        .map(|a| a.automaton.name.clone())
        .collect::<BTreeSet<_>>();
    JointModel::whole(&by_name, scope)
}

/// Follow admitted reactions to completion, choosing the branch in which an
/// unresolved reference finds its record. Each dispatch also goes through
/// the walk's packed representation, so losing its ancestry cannot hide a loop.
fn run(model: &JointModel, entity: &str, name: &str) -> JointState {
    let initial = model.inits().next().unwrap();
    let action = TemperModelAction {
        name: name.into(),
        target_state: model.models[entity]
            .transitions
            .iter()
            .find(|t| t.name == name)
            .unwrap()
            .to_state
            .clone(),
        params: BTreeMap::new(),
    };
    let mut state = model
        .commit(initial, entity, &action, &[], OUTSIDE)
        .into_iter()
        .max_by_key(|(s, _)| s.inflight.len())
        .unwrap()
        .0;
    let mut packer = Packer::default();
    while !state.inflight.is_empty() {
        let packed = packer.pack(&state);
        assert_eq!(packer.unpack(&packed), state);
        state = packer.unpack(&packed);
        let dispatch = state.inflight.remove(0);
        state = model
            .deliver(state, &dispatch, "reaction")
            .into_iter()
            .max_by_key(|(s, _, _)| s.inflight.len())
            .unwrap()
            .0;
    }
    state
}

/// A `Steps` spec whose action `A{i}` moves it from `S{i}` to `S{i+1}` and
/// triggers the next action on itself, so firing `A0` sets off a cascade of
/// `actions` distinct steps.
fn stepping_ioa(actions: usize) -> String {
    let states = (0..=actions)
        .map(|index| format!("\"S{index}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut ioa = format!(
        "[automaton]\nname = \"Steps\"\nstates = [{states}]\ninitial = \"S0\"\nallow_indefinite_states = [{states}]\n"
    );
    for index in 0..actions {
        let next = index + 1;
        ioa.push_str(&format!(
            "\n[[action]]\nname = \"A{index}\"\nfrom = [\"S{index}\"]\nto = \"S{next}\"\n"
        ));
        if next < actions {
            ioa.push_str(&format!(
                "\n[[action.triggers]]\nname = \"then_a{next}\"\nkind = \"entity\"\ntarget_entity = \"Steps\"\ntarget_action = \"A{next}\"\n\n[action.triggers.resolve_target]\nkind = \"same_id\"\n"
            ));
        }
    }
    ioa
}

#[test]
fn a_long_cascade_of_distinct_steps_runs_to_its_end_on_a_small_thread() {
    // A cascade stops only at a repeated step, so one of distinct steps runs
    // as far as the spec goes, however small the checking thread's stack. A
    // thread that overflows its stack aborts the process, so this fails by
    // crashing.
    const ACTIONS: usize = 2_000;
    let steps = parse_automaton(&stepping_ioa(ACTIONS)).unwrap();
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn_scoped(scope, move || {
                let model = whole(&[&steps]);
                let after = run(&model, "Steps", "A0");
                assert_eq!(
                    after.records.get("Steps").unwrap().status,
                    format!("S{ACTIONS}")
                );
            })
            .expect("spawn the test thread")
            .join()
            .expect("the cascade ran to its end");
    });
}

#[test]
fn a_created_child_runs_before_the_next_made_step_repeats() {
    for (resolver, child_action, child_id, expected_runs) in [
        ("same_id", "Sprout", "", 1),
        ("create", "Sprout", "", 2),
        ("create", "Grow", "", 2),
        ("create_if_missing", "Sprout", "", 2),
        ("create_if_missing", "Grow", "", 2),
        ("create_if_missing", "Sprout", "original", 1),
        ("create_if_missing", "Grow", "original", 1),
    ] {
        let id_field = if resolver == "create_if_missing" {
            r#", id_field = "child_id""#
        } else {
            ""
        };
        let seed = parse_automaton(&format!(
            r#"
[automaton]
name = "Seed"
states = ["Sown"]
initial = "Sown"
allow_indefinite_states = ["Sown"]

[[state]]
name = "child_id"
type = "string"
initial = "{child_id}"

[[state]]
name = "sprouts"
type = "counter"
initial = 0

# The model holds only a counter something reads.
[[invariant]]
name = "counted"
assert = "sprouts >= 0"

[[action]]
name = "Sprout"
from = ["Sown"]
to = "Sown"
effect = ["sprouts += 1"]

[[action.triggers]]
name = "sprout_again"
kind = "entity"
target_entity = "Seed"
target_action = "{child_action}"
resolve_target = {{ kind = "{resolver}"{id_field} }}

[[action.triggers]]
name = "record_sprout"
kind = "entity"
target_entity = "Observer"
target_action = "Record"
resolve_target = {{ kind = "same_id" }}

[[action]]
name = "Grow"
from = ["Sown"]
to = "Sown"

[[action.triggers]]
name = "sprout_on_child"
kind = "entity"
target_entity = "Seed"
target_action = "Sprout"
resolve_target = {{ kind = "same_id" }}
"#
        ))
        .unwrap();
        let observer = parse_automaton(
            r#"
[automaton]
name = "Observer"
states = ["Watching"]
initial = "Watching"
allow_indefinite_states = ["Watching"]

[[state]]
name = "records"
type = "counter"
initial = 0

# The model holds only a counter something reads.
[[invariant]]
name = "counted"
assert = "records >= 0"

[[action]]
name = "Record"
from = ["Watching"]
to = "Watching"
effect = ["records += 1"]
"#,
        )
        .unwrap();
        let model = whole(&[&seed, &observer]);
        let after = run(&model, "Seed", "Sprout");

        // The joint model accumulates effects in one slice per type. A found
        // step runs once; a create admits one made step too, then stops the
        // next made step. Both admitted actions must fire their reactions.
        assert_eq!(
            after.records["Seed"].counters["sprouts"], expected_runs,
            "{resolver} {child_action} with id {child_id:?}: effects of admitted Sprout actions"
        );
        assert_eq!(
            after.records["Observer"].counters["records"], expected_runs,
            "{resolver} {child_action} with id {child_id:?}: downstream reactions of admitted Sprout actions"
        );
    }
}

#[test]
fn a_made_step_returning_through_another_type_runs_once() {
    let after = cross_type_same_id_cascade("");

    // Seed(original).Start -> Seed(g).Grow -> Relay(g).Forward -> Seed(g).Grow.
    // The returning Seed is still made: its second Grow and its reactions
    // must be stopped, just as production's InlineChain stops them.
    assert_eq!(after.records["Seed"].counters["growths"], 1);
    assert_eq!(after.records["Observer"].counters["records"], 1);
    assert_eq!(after.records["Relay"].counters["forwards"], 1);
}

#[test]
fn a_new_create_after_cross_type_same_id_starts_its_own_id_origin() {
    let after = cross_type_same_id_cascade(
        r#"
[[action.triggers]]
name = "make_relay"
kind = "entity"
target_entity = "Relay"
target_action = "Forward"
resolve_target = { kind = "create" }
"#,
    );

    // Relay(g) is found even though Seed(g) is made, so making Relay(h)
    // admits a second Forward. Seed(h) is found too: the new id's origin
    // must not inherit Seed(g)'s made identity and suppress its first Grow.
    assert_eq!(after.records["Relay"].counters["forwards"], 2);
    assert_eq!(after.records["Seed"].counters["growths"], 2);
    assert_eq!(after.records["Observer"].counters["records"], 2);
}

fn cross_type_same_id_cascade(relay_trigger: &str) -> JointState {
    let seed = parse_automaton(
        r#"
[automaton]
name = "Seed"
states = ["Sown"]
initial = "Sown"
allow_indefinite_states = ["Sown"]

[[state]]
name = "growths"
type = "counter"
initial = 0

# The model holds only a counter something reads.
[[invariant]]
name = "counted"
assert = "growths >= 0"

[[action]]
name = "Start"
from = ["Sown"]
to = "Sown"

[[action.triggers]]
name = "make_seed"
kind = "entity"
target_entity = "Seed"
target_action = "Grow"
resolve_target = { kind = "create" }

[[action]]
name = "Grow"
from = ["Sown"]
to = "Sown"
effect = ["growths += 1"]

[[action.triggers]]
name = "record_growth"
kind = "entity"
target_entity = "Observer"
target_action = "Record"
resolve_target = { kind = "same_id" }

[[action.triggers]]
name = "forward_growth"
kind = "entity"
target_entity = "Relay"
target_action = "Forward"
resolve_target = { kind = "same_id" }
"#,
    )
    .unwrap();
    let relay = parse_automaton(&format!(
        r#"
[automaton]
name = "Relay"
states = ["Ready"]
initial = "Ready"
allow_indefinite_states = ["Ready"]

[[state]]
name = "forwards"
type = "counter"
initial = 0

# The model holds only a counter something reads.
[[invariant]]
name = "counted"
assert = "forwards >= 0"

[[action]]
name = "Forward"
from = ["Ready"]
to = "Ready"
effect = ["forwards += 1"]

[[action.triggers]]
name = "return_to_seed"
kind = "entity"
target_entity = "Seed"
target_action = "Grow"
resolve_target = {{ kind = "same_id" }}

{relay_trigger}
"#,
    ))
    .unwrap();
    let observer = parse_automaton(
        r#"
[automaton]
name = "Observer"
states = ["Watching"]
initial = "Watching"
allow_indefinite_states = ["Watching"]

[[state]]
name = "records"
type = "counter"
initial = 0

# The model holds only a counter something reads.
[[invariant]]
name = "counted"
assert = "records >= 0"

[[action]]
name = "Record"
from = ["Watching"]
to = "Watching"
effect = ["records += 1"]
"#,
    )
    .unwrap();
    run(&whole(&[&seed, &relay, &observer]), "Seed", "Start")
}
