use std::collections::BTreeMap;

use serde_json::json;

use super::*;
use crate::table::guard::{Related, RelatedMap};

const ORDER: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "items"
type = "counter"
initial = 0

[[state]]
name = "parent_id"
type = "string"
initial = ""

[[action]]
name = "Submit"
kind = "input"
from = ["Draft"]
to = "Submitted"

[[invariant]]
name = "SubmittedHaveItems"
assert = "status == 'Submitted' => items > 0"

[[invariant]]
name = "SubmittedUnderOpenParent"
assert = "status == 'Submitted' => Parent[parent_id].status == 'Open'"

[[invariant]]
name = "SameNoteAsParent"
assert = "status == 'Submitted' => Parent[parent_id].parent_id == parent_id"
"#;

fn ctx(items: usize, parent_id: &str, parent_status: &str) -> EvalContext {
    let mut related = RelatedMap::new();
    related.insert(
        ("Parent".to_string(), "parent_id".to_string()),
        Related::Statuses(vec![Some(parent_status.to_string())]),
    );
    EvalContext {
        counters: BTreeMap::from([("items".to_string(), items)]),
        fields: BTreeMap::from([("parent_id".to_string(), json!(parent_id))]),
        related,
        ..EvalContext::default()
    }
}

fn names(invariants: &[(&MoveInvariant, GuardFailure)]) -> Vec<String> {
    invariants.iter().map(|(i, _)| i.name.clone()).collect()
}

#[test]
fn an_invariant_reading_a_related_field_is_not_checked_on_moves() {
    let table = TransitionTable::from_ioa_source(ORDER);
    let checked: Vec<&str> = table.invariants.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(checked, ["SubmittedHaveItems", "SubmittedUnderOpenParent"]);
}

#[test]
fn a_move_that_makes_a_holding_invariant_false_breaks_it() {
    let table = TransitionTable::from_ioa_source(ORDER);
    let before = ctx(0, "p1", "Open");
    let across = table.invariants_across_move(("Draft", &before), ("Submitted", &before));
    let (broken, failure) = across.broken.expect("the move breaks an invariant");
    assert_eq!(broken.name, "SubmittedHaveItems");
    assert!(
        failure
            .found
            .contains(&("items".to_string(), "0".to_string())),
        "{:?}",
        failure.found
    );
    assert!(across.already_broken.is_empty());

    let after = ctx(1, "p1", "Open");
    let across = table.invariants_across_move(("Draft", &before), ("Submitted", &after));
    assert!(across.broken.is_none(), "{:?}", across.broken);
}

#[test]
fn an_invariant_that_did_not_hold_before_the_move_does_not_stop_it() {
    let table = TransitionTable::from_ioa_source(ORDER);
    let state = ctx(0, "p1", "Open");
    let across = table.invariants_across_move(("Submitted", &state), ("Submitted", &state));
    assert!(across.broken.is_none(), "{:?}", across.broken);
    assert_eq!(names(&across.already_broken), ["SubmittedHaveItems"]);
}

#[test]
fn a_related_status_is_checked_unless_the_move_points_elsewhere() {
    let table = TransitionTable::from_ioa_source(ORDER);
    let before = ctx(1, "p1", "Closed");
    let across = table.invariants_across_move(("Draft", &before), ("Submitted", &before));
    let (broken, _) = across.broken.expect("a closed parent breaks the invariant");
    assert_eq!(broken.name, "SubmittedUnderOpenParent");

    // The statuses were read for p1; after the move the order points at p2,
    // whose status nobody read, so the invariant is not judged on p1's.
    let after = ctx(1, "p2", "Closed");
    let across = table.invariants_across_move(("Draft", &before), ("Submitted", &after));
    assert!(across.broken.is_none(), "{:?}", across.broken);
}

#[test]
fn a_related_status_is_checked_unless_the_move_changes_its_reference_list() {
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "parent_ids"
type = "list"
initial = []

[[action]]
name = "Submit"
kind = "input"
from = ["Draft"]
to = "Submitted"

[[invariant]]
name = "SubmittedUnderOpenParents"
assert = "status == 'Submitted' => Parent[parent_ids].status == 'Open'"
"#,
    );
    assert!(table.initial_values.lists.contains_key("parent_ids"));

    // References can be declared lists or JSON array fields. Dispatch resolved
    // the old parents, so their statuses stay the same throughout the move.
    for declared_list in [true, false] {
        let mut before = EvalContext::default();
        if declared_list {
            before
                .lists
                .insert("parent_ids".into(), vec!["p1".into(), "p2".into()]);
        } else {
            before
                .fields
                .insert("parent_ids".into(), json!(["p1", "p2"]));
        }
        before.related.insert(
            ("Parent".into(), "parent_ids".into()),
            Related::Statuses(vec![Some("Closed".into()), Some("Open".into())]),
        );
        let across = table.invariants_across_move(("Draft", &before), ("Submitted", &before));
        let (broken, _) = across
            .broken
            .expect("an unchanged closed parent breaks the invariant");
        assert_eq!(broken.name, "SubmittedUnderOpenParents");

        let mut after = before.clone();
        if declared_list {
            after.lists.insert("parent_ids".into(), vec!["p2".into()]);
        } else {
            after.fields.insert("parent_ids".into(), json!(["p2"]));
        }
        let across = table.invariants_across_move(("Draft", &before), ("Submitted", &after));
        assert!(
            across.broken.is_none(),
            "declared_list={declared_list}: {:?}",
            across.broken
        );
        assert!(across.already_broken.is_empty());
    }
}
