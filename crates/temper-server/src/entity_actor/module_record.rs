//! The record as a WASM module reads it.

use std::collections::BTreeMap;

use serde::Serialize;

use super::super::LastEvent;
use super::{EntityState, ProcessedIdempotencyKey, StatusClock};

/// One past move as a module reads it: what moved and when, without what it was told. A move's parameters are as big as
/// what it was sent, and a record's recent events keep them, so a record that takes large values holds many copies of
/// them. A Katagami app's module was handed 17 MB for an app of 642 KB, and ran out of memory (1 Nov 2026).
#[derive(Serialize)]
struct ModuleEvent<'a> {
    action: &'a str,
    from_status: &'a str,
    to_status: &'a str,
    timestamp: chrono::DateTime<chrono::Utc>,
}

/// The record as a module reads it: [`EntityState`], with each recent event as a [`ModuleEvent`]. It borrows the record,
/// so the parameters are never copied just to be left out.
#[derive(Serialize)]
struct ModuleRecord<'a> {
    entity_type: &'a str,
    entity_id: &'a str,
    status: &'a str,
    item_count: usize,
    counters: &'a BTreeMap<String, usize>,
    booleans: &'a BTreeMap<String, bool>,
    lists: &'a BTreeMap<String, Vec<String>>,
    fields: &'a serde_json::Value,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    spawn_reservations: &'a BTreeMap<String, String>,
    events: Vec<ModuleEvent<'a>>,
    total_event_count: usize,
    events_since_snapshot: usize,
    last_snapshot_sequence_nr: u64,
    sequence_nr: u64,
    processed_idempotency_keys: &'a BTreeMap<String, ProcessedIdempotencyKey>,
    status_clock: &'a StatusClock,
    #[serde(flatten)]
    last_event: &'a LastEvent,
}

impl EntityState {
    /// What a module is given as `entity_state`. Its size follows the record's fields, not how many large moves it took
    /// lately. A module wanting what started it reads `trigger_params`, and a value that lasts reads `fields`.
    pub(crate) fn for_module(&self) -> serde_json::Value {
        let record = ModuleRecord {
            entity_type: &self.entity_type,
            entity_id: &self.entity_id,
            status: &self.status,
            item_count: self.item_count,
            counters: &self.counters,
            booleans: &self.booleans,
            lists: &self.lists,
            fields: &self.fields,
            spawn_reservations: &self.spawn_reservations,
            events: self
                .events
                .iter()
                .map(|event| ModuleEvent {
                    action: &event.action,
                    from_status: &event.from_status,
                    to_status: &event.to_status,
                    timestamp: event.timestamp,
                })
                .collect(),
            total_event_count: self.total_event_count,
            events_since_snapshot: self.events_since_snapshot,
            last_snapshot_sequence_nr: self.last_snapshot_sequence_nr,
            sequence_nr: self.sequence_nr,
            processed_idempotency_keys: &self.processed_idempotency_keys,
            status_clock: &self.status_clock,
            last_event: &self.last_event,
        };
        serde_json::to_value(record).unwrap_or_default()
    }
}
