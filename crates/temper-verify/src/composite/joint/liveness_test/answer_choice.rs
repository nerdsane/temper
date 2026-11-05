//! A module's answer whose triggers choose the next move from the values
//! it carried. The runtime copies a move's parameters into the record's
//! fields before its triggers' guards read them, so guards that split on
//! those values fire exactly one trigger.

use super::*;

/// A feed: `look` reports on one entry, and the report's guards choose to
/// drop, skip or pick it, which clears it. `kept` and `most` are `int`
/// fields, which live among the copied fields; `in_bank` is no field.
fn feed(choices: &str) -> String {
    format!(
        r#"
[automaton]
name = "Feed"
states = ["Resting", "Running"]
initial = "Resting"

[[state]]
name = "has_pending"
type = "bool"
initial = false

[[state]]
name = "kept"
type = "int"
initial = 0

[[state]]
name = "most"
type = "int"
initial = 0

[[action]]
name = "Now"
kind = "input"
from = ["Resting"]
to = "Running"

[[action.triggers]]
name = "look"
kind = "wasm"
module = "look"
on_success = "Reported"

[[action]]
name = "Reported"
kind = "input"
from = ["Running"]
params = ["has_pending", "in_bank", "kept", "most"]
effect = ["has_pending = params.has_pending"]
{choices}
[[action]]
name = "Drop"
kind = "input"
from = ["Running"]
effect = ["has_pending = false"]

[[action]]
name = "Skip"
kind = "input"
from = ["Running"]
effect = ["has_pending = false"]

[[action]]
name = "Pick"
kind = "input"
from = ["Running"]
effect = ["has_pending = false"]

[[liveness]]
name = "a_looked_at_entry_is_handled"
eventually = "!has_pending"
"#
    )
}

fn choice(name: &str, guard: &str, target: &str) -> String {
    format!(
        r#"
[[action.triggers]]
name = "{name}"
kind = "entity"
guard = "{guard}"
target_entity = "Feed"
target_action = "{target}"
resolve_target = {{ kind = "same_id" }}
"#
    )
}

fn check_feed(choices: &[String]) -> JointLivenessResult {
    let automaton = parse_automaton(&feed(&choices.concat())).expect("fixture parses");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&[&automaton], None, bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

fn drop_skip_pick() -> Vec<String> {
    vec![
        choice("choose_drop", "has_pending && in_bank", "Drop"),
        choice(
            "choose_skip",
            "has_pending && !in_bank && kept >= most",
            "Skip",
        ),
        choice(
            "choose_pick",
            "has_pending && !in_bank && kept < most",
            "Pick",
        ),
    ]
}

#[test]
fn guards_that_split_on_an_answers_values_fire_one_choice() {
    let result = check_feed(&drop_skip_pick());
    assert!(result.holds(), "{result}");
}

#[test]
fn an_answer_no_guard_chooses_for_leaves_its_entry_pending() {
    // Without the pick, an entry not in the bank with room in its family
    // is chosen by no guard.
    let mut choices = drop_skip_pick();
    choices.pop();
    let result = check_feed(&choices);
    let LivenessOutcome::Stuck { cases, .. } = &result.outcome else {
        panic!("an entry with room left must stay pending: {result}");
    };
    assert!(
        cases.iter().any(|case| case
            .path
            .iter()
            .filter_map(|step| step.action.as_deref())
            .any(|action| action.contains("Reported") && action.contains("in_bank=false"))),
        "{result}"
    );
}
