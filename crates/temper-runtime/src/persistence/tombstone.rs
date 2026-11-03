//! The kernel's deletion tombstone and the one rule that recognises it.
//!
//! The kernel's delete journals its tombstone under the event type
//! [`TOMBSTONE_EVENT_TYPE`]. A spec action may carry the same name, so the name
//! never decides deletion on its own: every entity event payload names its
//! [`EventKind`] under [`EVENT_KIND_KEY`], and only the kernel's delete writes
//! [`EventKind::Tombstone`]. Store queries that probe for tombstones mirror
//! [`PersistenceEnvelope::is_tombstone`] in SQL.

use serde::{Deserialize, Serialize};

use super::PersistenceEnvelope;

/// Event type the kernel's delete journals its tombstone under.
pub const TOMBSTONE_EVENT_TYPE: &str = "Deleted";

/// Payload key holding the [`EventKind`] of an entity event.
pub const EVENT_KIND_KEY: &str = "kind";

/// Which kind of entity event the kernel wrote.
///
/// Serialized into every entity event payload. Events journaled before the
/// field existed have no kind; [`PersistenceEnvelope::is_tombstone`] reads them
/// by the exact shape the kernel's delete wrote then.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A spec action, a field update or a creation.
    #[default]
    Transition,
    /// The kernel's delete: the entity no longer exists.
    Tombstone,
}

impl PersistenceEnvelope {
    /// Whether this event is the kernel's deletion tombstone.
    ///
    /// An event with a kind is a tombstone exactly when its kind says so. An
    /// event without one predates the field: the kernel's delete then wrote
    /// `to_status` `Deleted`, empty `params` and no idempotency key, so only that
    /// shape is a tombstone. A spec action named `Deleted` written then is read
    /// as one only if it had all three; written now, it has a kind.
    pub fn is_tombstone(&self) -> bool {
        if self.event_type != TOMBSTONE_EVENT_TYPE {
            return false;
        }
        match self.payload.get(EVENT_KIND_KEY) {
            Some(kind) => kind.as_str() == Some("tombstone"),
            None => {
                self.payload
                    .get("to_status")
                    .and_then(serde_json::Value::as_str)
                    == Some(TOMBSTONE_EVENT_TYPE)
                    && self
                        .payload
                        .get("params")
                        .and_then(serde_json::Value::as_object)
                        .is_some_and(serde_json::Map::is_empty)
                    && self
                        .payload
                        .get("idempotency_key")
                        .is_none_or(serde_json::Value::is_null)
            }
        }
    }
}

/// Whether a journal holds the kernel's tombstone. The tombstone is terminal,
/// so a journal that holds one anywhere is a deleted entity's.
pub fn journal_is_deleted(events: &[PersistenceEnvelope]) -> bool {
    events.iter().any(PersistenceEnvelope::is_tombstone)
}

#[cfg(test)]
#[path = "tombstone_test.rs"]
mod tests;
