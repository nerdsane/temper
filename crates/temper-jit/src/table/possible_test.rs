//! Tests for whether a stored record can make a move (`blocked`, `standing`,
//! `move_conditions`, `stored_context`).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::*;

const PLACE: &str = r#"
[automaton]
name = "Place"
states = ["Unplaced", "Bringing", "Placed", "Retired"]
initial = "Unplaced"

[[state]]
name = "ready"
type = "bool"
initial = false

[[state]]
name = "brought"
type = "counter"
initial = 0

[[action]]
name = "Bring"
kind = "input"
from = ["Unplaced", "Placed"]
to = "Bringing"
guard = "ready"
params = [{ name = "brought", type = "counter" }]

[[action.constraints]]
kind = "param_greater_than_field"
param = "brought"
field = "brought"

[[action]]
name = "Placed"
kind = "input"
from = ["Bringing"]
to = "Placed"

[[action]]
name = "Retire"
kind = "input"
from = ["Placed"]
to = "Retired"
guard = "brought > 2"

[[carry]]
from = "Placed"
to = "Unplaced"
set = { ready = true }
"#;

fn table(source: &str) -> TransitionTable {
    TransitionTable::from_ioa_source(source)
}

fn values(pairs: Value) -> BTreeMap<String, Value> {
    pairs
        .as_object()
        .expect("an object")
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

#[test]
fn a_record_that_can_move_is_not_blocked() {
    let place = table(PLACE);
    let ctx = place.stored_context(&values(json!({ "ready": true, "brought": 2 })));
    assert_eq!(place.blocked("Placed", &ctx, "Bring"), None);
}

#[test]
fn a_blocked_record_says_why() {
    let place = table(PLACE);
    let ready = place.stored_context(&values(json!({ "ready": true, "brought": 2 })));
    assert_eq!(
        place.blocked("Bringing", &ready, "Bring"),
        Some(Blocked::NotFrom)
    );
    assert_eq!(
        place.blocked("Placed", &ready, "Leave"),
        Some(Blocked::NoAction)
    );

    let not_ready = place.stored_context(&values(json!({ "brought": 2 })));
    match place.blocked("Placed", &not_ready, "Bring") {
        Some(Blocked::Guard(Some(failure))) => {
            assert!(failure.expr.contains("ready"), "{failure:?}")
        }
        other => panic!("the guard on ready blocks Bring: {other:?}"),
    }
}

/// A guard that reads the caller's parameters may hold for some call, so it
/// does not block a stored record; its parts that read none still do.
#[test]
fn a_guard_on_the_callers_parameters_blocks_only_through_its_other_parts() {
    let retire_when = |guard: &str| {
        table(&PLACE.replace(
            "guard = \"brought > 2\"",
            &format!("guard = \"{guard}\"\nparams = [{{ name = \"rain\", type = \"counter\" }}]"),
        ))
    };
    let place = retire_when("params.rain >= 50");
    let stored = place.stored_context(&values(json!({})));
    assert_eq!(place.blocked("Placed", &stored, "Retire"), None);

    let place = retire_when("ready && params.rain >= 50");
    let ready = place.stored_context(&values(json!({ "ready": true })));
    assert_eq!(place.blocked("Placed", &ready, "Retire"), None);
    let not_ready = place.stored_context(&values(json!({})));
    assert!(
        matches!(
            place.blocked("Placed", &not_ready, "Retire"),
            Some(Blocked::Guard(_))
        ),
        "ready is false whatever the call"
    );

    // The parts that read no parameter decide in every branch.
    let place = retire_when("(ready && params.rain >= 50) || brought > 2");
    let neither = place.stored_context(&values(json!({ "brought": 1 })));
    assert!(
        matches!(
            place.blocked("Placed", &neither, "Retire"),
            Some(Blocked::Guard(_))
        ),
        "neither branch can hold whatever the call"
    );
    assert_eq!(place.blocked("Placed", &ready, "Retire"), None);
    let brought = place.stored_context(&values(json!({ "brought": 3 })));
    assert_eq!(place.blocked("Placed", &brought, "Retire"), None);
    let place = retire_when("!(params.rain >= 50 && ready)");
    assert_eq!(place.blocked("Placed", &ready, "Retire"), None);
}

#[test]
fn a_record_stands_free_rests_stuck_or_gone() {
    let place = table(PLACE);
    let ready = place.stored_context(&values(json!({ "ready": true })));
    assert_eq!(place.standing("Placed", &ready), Standing::Free);
    assert_eq!(place.standing("Retired", &ready), Standing::Rests);
    assert_eq!(place.standing("Lost", &ready), Standing::Gone);

    let not_ready = place.stored_context(&values(json!({ "brought": 1 })));
    match place.standing("Placed", &not_ready) {
        Standing::Stuck(blocked) => {
            let moves: Vec<&str> = blocked.iter().map(|(action, _)| action.as_str()).collect();
            assert_eq!(moves, ["Bring", "Retire"]);
            assert!(
                blocked
                    .iter()
                    .all(|(_, why)| matches!(why, Blocked::Guard(Some(_)))),
                "{blocked:?}"
            );
        }
        other => panic!("neither Bring nor Retire is possible: {other:?}"),
    }
    // One move is enough.
    let many = place.stored_context(&values(json!({ "brought": 3 })));
    assert_eq!(place.standing("Placed", &many), Standing::Free);
}

#[test]
fn a_carry_is_a_move_for_the_platform_not_a_way_forward() {
    let place = table(PLACE);
    let carry = "carry:Placed:1";
    assert!(!place.caller_moves().contains(&carry));
    assert!(place.starts_from(carry, "Placed"));
    let stuck = place.stored_context(&values(json!({ "brought": 1 })));
    assert_eq!(place.blocked("Placed", &stuck, carry), None);
    assert!(matches!(
        place.standing("Placed", &stuck),
        Standing::Stuck(_)
    ));
}

#[test]
fn stored_values_take_the_kind_the_version_declares() {
    let place = table(PLACE);
    let ctx = place.stored_context(&values(json!({ "ready": true, "brought": 3, "note": "x" })));
    assert_eq!(ctx.booleans.get("ready"), Some(&true));
    assert_eq!(ctx.counters.get("brought"), Some(&3));
    assert_eq!(ctx.fields.get("note"), Some(&json!("x")));
    // A value not of its declared kind stays a field, as stored.
    let odd = place.stored_context(&values(json!({ "ready": "yes" })));
    assert!(odd.booleans.is_empty());
    assert_eq!(odd.fields.get("ready"), Some(&json!("yes")));
}

#[test]
fn a_move_reads_its_guards_and_constraint_fields() {
    let place = table(PLACE);
    let reads: Vec<String> = place.move_reads("Bring").into_iter().collect();
    assert_eq!(reads, ["brought", "ready"]);
    assert!(place.move_reads("Placed").is_empty());
}

#[test]
fn move_conditions_change_with_what_decides_the_move() {
    let place = table(PLACE);
    assert_eq!(
        place.move_conditions("Bring"),
        table(PLACE).move_conditions("Bring")
    );
    // Another action changing leaves Bring's conditions alone.
    let other = table(&PLACE.replace(
        "from = [\"Bringing\"]",
        "from = [\"Bringing\", \"Unplaced\"]",
    ));
    assert_eq!(
        place.move_conditions("Bring"),
        other.move_conditions("Bring")
    );

    for changed in [
        PLACE.replace("from = [\"Unplaced\", \"Placed\"]", "from = [\"Unplaced\"]"),
        PLACE.replace("guard = \"ready\"", "guard = \"!ready\""),
        PLACE.replace("param_greater_than_field", "param_not_equals_field"),
        PLACE.replace(
            "name = \"ready\"\ntype = \"bool\"\ninitial = false",
            "name = \"ready\"\ntype = \"bool\"\ninitial = true",
        ),
    ] {
        assert_ne!(
            place.move_conditions("Bring"),
            table(&changed).move_conditions("Bring"),
            "{changed}"
        );
    }
}
