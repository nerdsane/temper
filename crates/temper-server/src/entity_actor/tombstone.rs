//! The kernel's deletion tombstone: the event a delete writes, and what
//! applying it does to a record.

use super::types::{EntityEvent, EntityState};

impl EntityEvent {
    /// The kernel's deletion tombstone for a record last in `from_status`.
    pub(crate) fn tombstone(from_status: String) -> Self {
        Self {
            action: temper_runtime::persistence::TOMBSTONE_EVENT_TYPE.to_string(),
            from_status,
            to_status: "Deleted".to_string(),
            timestamp: temper_runtime::scheduler::sim_now(),
            params: serde_json::json!({}),
            idempotency_key: None,
            outcome: None,
            origin: crate::entity_actor::EventOrigin::internal(),
            kind: temper_runtime::persistence::EventKind::Tombstone,
        }
    }
}

impl EntityState {
    /// Apply the kernel's deletion tombstone: the record is `Deleted`.
    pub(crate) fn record_tombstone(&mut self, tombstone: EntityEvent) {
        debug_assert_eq!(
            tombstone.kind,
            temper_runtime::persistence::EventKind::Tombstone
        );
        self.status = tombstone.to_status.clone();
        if let Some(obj) = self.fields.as_object_mut() {
            obj.insert(
                "Status".to_string(),
                serde_json::Value::String(self.status.clone()),
            );
        }
        self.push_event_bounded(tombstone);
    }

    /// Whether the kernel's delete removed this record: its last event is the
    /// kernel's tombstone, which is terminal. Only that removes a record's
    /// catalog row and its id from the entity index. A spec action into a
    /// state named `Deleted` is an ordinary transition, and its record stays
    /// listed and readable like any other.
    pub(crate) fn is_tombstoned(&self) -> bool {
        self.events
            .back()
            .is_some_and(|event| event.kind == temper_runtime::persistence::EventKind::Tombstone)
    }
}
