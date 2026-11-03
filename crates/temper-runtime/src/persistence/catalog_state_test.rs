use super::*;
use serde_json::json;

fn record_state(fields: Value) -> Value {
    json!({
        "entity_type": "Note",
        "entity_id": "note-1",
        "status": "Draft",
        "counters": {"edits": 2},
        "fields": fields,
        "events": [],
        "sequence_nr": 4,
    })
}

#[test]
fn stored_state_leaves_out_the_fields_the_column_holds() {
    let body = "x".repeat(4096);
    let fields = json!({"Title": "Plan", "Body": body});
    let state = record_state(fields.clone());

    let stored = catalog_state_for_storage(&fields, &state);

    assert!(stored.get("fields").is_none());
    assert!(!stored.to_string().contains(&body));
    assert_eq!(stored["counters"]["edits"], 2);
    assert_eq!(catalog_state_from_storage(&fields, stored), state);
}

#[test]
fn stored_state_keeps_fields_kept_out_of_the_query_plane() {
    let record_fields = json!({"Title": "Plan", "Heartbeat": "t-9"});
    let column = json!({"Title": "Plan"});
    let state = record_state(record_fields);

    let stored = catalog_state_for_storage(&column, &state);

    assert_eq!(stored["fields"], json!({"Heartbeat": "t-9"}));
    assert_eq!(catalog_state_from_storage(&column, stored), state);
}

#[test]
fn stored_state_keeps_a_field_the_column_holds_differently() {
    let column = json!({"Title": "Old"});
    let state = record_state(json!({"Title": "New"}));

    let stored = catalog_state_for_storage(&column, &state);

    assert_eq!(stored["fields"], json!({"Title": "New"}));
    assert_eq!(catalog_state_from_storage(&column, stored), state);
}

#[test]
fn a_row_written_with_every_field_in_its_state_reads_unchanged() {
    let column = json!({"Title": "Plan"});
    let state = record_state(json!({"Title": "Plan", "Heartbeat": "t-9"}));

    assert_eq!(catalog_state_from_storage(&column, state.clone()), state);
}

#[test]
fn a_state_without_fields_reads_the_column() {
    let column = json!({"Title": "Plan"});
    let stored = json!({"status": "Draft"});

    let state = catalog_state_from_storage(&column, stored);

    assert_eq!(state["fields"], column);
}

#[test]
fn values_that_are_not_objects_are_stored_as_they_are() {
    let state = record_state(json!({"Title": "Plan"}));
    assert_eq!(catalog_state_for_storage(&Value::Null, &state), state);
    assert_eq!(
        catalog_state_for_storage(&json!({}), &json!("raw")),
        json!("raw")
    );
    assert_eq!(
        catalog_state_from_storage(&json!({}), json!("raw")),
        json!("raw")
    );
}
