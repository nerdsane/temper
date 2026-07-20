use super::*;

fn make_state(entity_type: &str, entity_id: &str) -> EntityState {
    EntityState {
        entity_type: entity_type.into(),
        entity_id: entity_id.into(),
        status: "Initial".into(),
        item_count: 0,
        counters: std::collections::BTreeMap::new(),
        booleans: std::collections::BTreeMap::new(),
        lists: std::collections::BTreeMap::new(),
        fields: serde_json::json!({}),
        events: std::collections::VecDeque::new(),
        state_timeout_clock_reset_at: None,
        state_timeout_clock_reset_version: None,
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
    }
}

mod fields;
mod processing;
mod references;
mod scheduling;
