//! Tests for the entity actor types.

use super::*;
use serde_json::json;

#[test]
fn durable_keys_keep_their_action_and_older_snapshots_still_load() {
    let json = json!({
        "entity_type": "Task",
        "entity_id": "task-1",
        "status": "Open",
        "item_count": 0,
        "fields": {},
        "processed_idempotency_keys": {
            "older": 3,
            "newer": { "sequence": 4, "action": "Note" }
        },
    });
    let state: EntityState = serde_json::from_value(json).unwrap();
    let older = &state.processed_idempotency_keys["older"];
    assert_eq!((older.sequence, older.action.as_deref()), (3, None));
    assert!(
        older.was_for("Close"),
        "an older key has no action to compare"
    );
    let newer = &state.processed_idempotency_keys["newer"];
    assert!(newer.was_for("Note"));
    assert!(!newer.was_for("Close"));

    let round_trip: EntityState =
        serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert_eq!(
        round_trip.processed_idempotency_keys,
        state.processed_idempotency_keys
    );
}

/// A record that takes large values keeps them in its recent events: an app's description, twice per install, made a
/// module's context 17 MB and ran it out of memory (1 Nov 2026).
#[test]
fn a_module_reads_the_record_without_what_its_past_moves_were_told() {
    let sent = "x".repeat(1_000_000);
    let event = |action: &str| {
        json!({
            "action": action,
            "from_status": "Open",
            "to_status": "Open",
            "timestamp": "2026-11-01T23:29:05Z",
            "params": { "description": sent },
        })
    };
    let state: EntityState = serde_json::from_value(json!({
        "entity_type": "App",
        "entity_id": "a1",
        "status": "Open",
        "item_count": 2,
        "counters": { "version": 7 },
        "booleans": { "broke": false },
        "lists": { "members": ["m"] },
        "fields": { "description": "the one it runs" },
        "spawn_reservations": { "child": "3:0" },
        "events": [event("Installed"), event("Installed"), event("Propose")],
        "total_event_count": 3,
        "sequence_nr": 3,
        "processed_idempotency_keys": { "k": { "sequence": 2, "action": "Propose" } },
    }))
    .unwrap();

    let whole = serde_json::to_value(&state).unwrap();
    let record = state.for_module();

    assert!(
        record.to_string().len() < 2_000,
        "{} bytes for a record of three million bytes of old parameters",
        record.to_string().len()
    );
    // What a module reads of past moves (an agent app's trajectory emitter): their names and times, oldest first.
    let moves: Vec<_> = record["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            (
                event["action"].as_str().unwrap(),
                event["timestamp"].as_str().unwrap(),
            )
        })
        .collect();
    let time = whole["events"][0]["timestamp"].as_str().unwrap();
    assert_eq!(
        moves,
        [("Installed", time), ("Installed", time), ("Propose", time)]
    );
    // Everything else stays as the whole record says it, including a field added to the record later.
    let others = |value: &serde_json::Value| {
        let mut object = value.as_object().unwrap().clone();
        object.remove("events");
        object
    };
    assert_eq!(others(&record), others(&whole));
}

#[test]
fn event_budget_is_based_on_snapshot_tail_not_lifetime_total() {
    let state = EntityState {
        entity_type: "Workspace".to_string(),
        entity_id: "workspace-1".to_string(),
        status: "Active".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: json!({}),
        events: VecDeque::new(),
        total_event_count: MAX_EVENTS_SINCE_SNAPSHOT + 50,
        events_since_snapshot: 2,
        last_snapshot_sequence_nr: MAX_EVENTS_SINCE_SNAPSHOT as u64 + 48,
        sequence_nr: MAX_EVENTS_SINCE_SNAPSHOT as u64 + 50,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
        last_event: Default::default(),
        status_clock: Default::default(),
        spawn_reservations: Default::default(),
    };

    assert!(state.can_accept_event());
}

#[test]
fn event_budget_rejects_when_snapshot_tail_reaches_cap() {
    let state = EntityState {
        entity_type: "Workspace".to_string(),
        entity_id: "workspace-1".to_string(),
        status: "Active".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: json!({}),
        events: VecDeque::new(),
        total_event_count: MAX_EVENTS_SINCE_SNAPSHOT,
        events_since_snapshot: MAX_EVENTS_SINCE_SNAPSHOT,
        last_snapshot_sequence_nr: 0,
        sequence_nr: MAX_EVENTS_SINCE_SNAPSHOT as u64,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
        last_event: Default::default(),
        status_clock: Default::default(),
        spawn_reservations: Default::default(),
    };

    assert!(!state.can_accept_event());
}

#[test]
fn entity_response_spec_governed_skipped_when_true() {
    let state = EntityState {
        entity_type: "Order".to_string(),
        entity_id: "o1".to_string(),
        status: "Draft".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: json!({}),
        events: VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
        last_event: Default::default(),
        status_clock: Default::default(),
        spawn_reservations: Default::default(),
    };
    let resp = EntityResponse {
        success: true,
        refusal: None,
        state,
        error: None,
        custom_effects: vec![],
        scheduled_actions: vec![],
        spawn_requests: vec![],
        spec_governed: true,
        refused: false,
        answered_action: None,
        trigger_failures: Vec::new(),
        store_unavailable: false,
        unrecorded_at: None,
    };
    let serialized = serde_json::to_string(&resp).unwrap();
    assert!(!serialized.contains("spec_governed"));
}
