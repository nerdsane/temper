use super::*;
use crate::persistence::EventMetadata;
use serde_json::json;

fn event(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 1,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: uuid::Uuid::nil(),
            causation_id: uuid::Uuid::nil(),
            correlation_id: uuid::Uuid::nil(),
            timestamp: chrono::DateTime::UNIX_EPOCH,
            actor_id: "default:Thing:t-1".to_string(),
        },
    }
}

fn payload(to_status: &str, params: serde_json::Value) -> serde_json::Value {
    json!({
        "action": "Deleted",
        "from_status": "Deleting",
        "to_status": to_status,
        "timestamp": "2026-10-24T00:00:00Z",
        "params": params,
    })
}

#[test]
fn the_kind_decides_for_events_that_carry_one() {
    let mut tombstone = payload("Deleted", json!({}));
    tombstone[EVENT_KIND_KEY] = json!("tombstone");
    assert!(event("Deleted", tombstone).is_tombstone());

    // A spec action named Deleted, landing in a state named Deleted, with no
    // params: the same shape the old delete wrote, told apart by its kind.
    let mut action = payload("Deleted", json!({}));
    action[EVENT_KIND_KEY] = json!("transition");
    assert!(!event("Deleted", action).is_tombstone());
}

#[test]
fn events_without_a_kind_are_read_by_the_old_delete_shape() {
    assert!(event("Deleted", payload("Deleted", json!({}))).is_tombstone());

    let with_params = payload("Deleted", json!({ "note": "gone", "deleted_at": "t" }));
    assert!(!event("Deleted", with_params).is_tombstone());

    assert!(!event("Deleted", payload("Purged", json!({}))).is_tombstone());

    let mut keyed = payload("Deleted", json!({}));
    keyed["idempotency_key"] = json!("k-1");
    assert!(!event("Deleted", keyed).is_tombstone());
}

#[test]
fn only_the_tombstone_event_type_can_be_a_tombstone() {
    let mut marked = payload("Deleted", json!({}));
    marked[EVENT_KIND_KEY] = json!("tombstone");
    assert!(!event("Purge", marked).is_tombstone());
}

#[test]
fn a_journal_is_deleted_when_it_holds_a_tombstone_anywhere() {
    let created = event("Created", json!({ "kind": "transition" }));
    let deleted_action = event("Deleted", payload("Gone", json!({ "note": "n" })));
    let tombstone = event("Deleted", payload("Deleted", json!({})));
    assert!(!journal_is_deleted(&[
        created.clone(),
        deleted_action.clone()
    ]));
    assert!(journal_is_deleted(&[created, tombstone, deleted_action]));
}
