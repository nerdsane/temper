//! Telling a projection that changes nothing from one that does.
//!
//! Every action moves an entity's sequence and event counters, a poll that
//! changes nothing else included. A synced database's change log records each
//! row update with the whole row before and after it, so rewriting a large
//! catalog row for such an action costs the change log twice the row. A
//! projection whose status, fields and state (less those counters) hash the
//! same as the stored row's leaves the catalog row alone; only its counters
//! go to the narrow `entity_catalog_counters` row.

use sha2::{Digest, Sha256};
use temper_runtime::persistence::{PersistenceError, storage_error};

use crate::driver::{Row, Transaction, params};

/// Keys of a projected state that count the entity's events rather than
/// describe it. Every action moves them, so they do not make a projection
/// differ, and a projection that changes nothing else stores only these.
const EVENT_COUNTER_KEYS: [&str; 4] = [
    "sequence_nr",
    "total_event_count",
    "events_since_snapshot",
    "last_snapshot_sequence_nr",
];

/// Store the event counters of a projection that matched its catalog row.
/// Only a newer sequence replaces stored counters.
const UPSERT_COUNTERS_SQL: &str = "\
INSERT INTO entity_catalog_counters (tenant, entity_type, entity_id, sequence_nr, counters) \
VALUES (?1, ?2, ?3, ?4, ?5) \
ON CONFLICT(tenant, entity_type, entity_id) DO UPDATE SET \
    sequence_nr = excluded.sequence_nr, \
    counters = excluded.counters \
WHERE entity_catalog_counters.sequence_nr < excluded.sequence_nr";

/// Drop an entity's counters row once its catalog row is written again (or
/// removed), so the catalog row's own sequence and state are current.
const DELETE_COUNTERS_SQL: &str = "\
DELETE FROM entity_catalog_counters WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3";

/// What a projection write compares against: the stored row's sequence and
/// hash, and the sequence of a newer unchanged projection if one was stored.
pub(super) struct ExistingProjection {
    catalog_sequence_nr: i64,
    /// The stored row's [`projection_hash`].
    pub(super) projection_hash: String,
    /// The sequence in `entity_catalog_counters`, when that row exists.
    pub(super) counters_sequence_nr: Option<i64>,
}

impl ExistingProjection {
    /// Read `(catalog sequence_nr, projection_hash, counters sequence_nr)`
    /// from the row's columns starting at `first`.
    pub(super) fn from_row(row: &Row, first: usize) -> Result<Self, PersistenceError> {
        Ok(Self {
            catalog_sequence_nr: row.get(first).map_err(storage_error)?,
            projection_hash: row.get(first + 1).map_err(storage_error)?,
            counters_sequence_nr: row.get(first + 2).map_err(storage_error)?,
        })
    }

    /// The newest projection this entity has had: a counters row is only
    /// ever newer than its catalog row.
    pub(super) fn sequence_nr(&self) -> i64 {
        self.counters_sequence_nr
            .unwrap_or(self.catalog_sequence_nr)
            .max(self.catalog_sequence_nr)
    }
}

/// Store a matching projection's sequence and event counters for the entity
/// `[tenant, entity_type, entity_id]`, inside the caller's write transaction.
pub(super) async fn store_event_counters(
    tx: &Transaction<'_>,
    [tenant, entity_type, entity_id]: [&str; 3],
    sequence_nr: i64,
    state: &serde_json::Value,
) -> Result<(), PersistenceError> {
    tx.execute(
        UPSERT_COUNTERS_SQL,
        params![
            tenant,
            entity_type,
            entity_id,
            sequence_nr,
            event_counters(state)
        ],
    )
    .await
    .map_err(storage_error)?;
    Ok(())
}

/// Drop the entity's counters row, inside the caller's write transaction.
pub(super) async fn drop_event_counters(
    tx: &Transaction<'_>,
    [tenant, entity_type, entity_id]: [&str; 3],
) -> Result<(), PersistenceError> {
    tx.execute(DELETE_COUNTERS_SQL, params![tenant, entity_type, entity_id])
        .await
        .map_err(storage_error)?;
    Ok(())
}

/// The event counters of a projected state, as the JSON object
/// `entity_catalog_counters.counters` holds.
fn event_counters(state: &serde_json::Value) -> String {
    let counters = state
        .as_object()
        .map(|entries| {
            EVENT_COUNTER_KEYS
                .iter()
                .filter_map(|key| {
                    entries
                        .get(*key)
                        .map(|value| (key.to_string(), value.clone()))
                })
                .collect::<serde_json::Map<_, _>>()
        })
        .unwrap_or_default();
    serde_json::Value::Object(counters).to_string()
}

/// Put stored event counters (from [`event_counters`]) into a catalog row's
/// state, so a read serves what writing the whole row would have stored.
pub(super) fn apply_event_counters(
    state: Option<&mut serde_json::Value>,
    counters: &str,
) -> Result<(), serde_json::Error> {
    let counters: serde_json::Map<String, serde_json::Value> = serde_json::from_str(counters)?;
    if let Some(entries) = state.and_then(serde_json::Value::as_object_mut) {
        entries.extend(counters);
    }
    Ok(())
}

/// Hash of what a catalog row says about its entity: the status, the fields
/// as stored, and the projected state without its event counters. Two
/// projections with the same hash write the same row apart from those
/// counters and `sequence_nr`.
pub(super) fn projection_hash(
    status: &str,
    fields_json: &str,
    state: &serde_json::Value,
) -> String {
    let mut hasher = HashWriter(Sha256::new());
    hasher.0.update(status.as_bytes());
    hasher.0.update(b"\n");
    hasher.0.update(fields_json.as_bytes());
    hasher.0.update(b"\n");
    match state.as_object() {
        Some(entries) => {
            for (key, value) in entries {
                if EVENT_COUNTER_KEYS.contains(&key.as_str()) {
                    continue;
                }
                hasher.0.update(key.as_bytes());
                hasher.0.update(b"=");
                write_json(&mut hasher, value);
                hasher.0.update(b"\n");
            }
        }
        None => write_json(&mut hasher, state),
    }
    format!("{:x}", hasher.0.finalize())
}

fn write_json(hasher: &mut HashWriter, value: &serde_json::Value) {
    // Writing into a hasher cannot fail, and a `Value` always serializes.
    serde_json::to_writer(hasher, value).expect("a JSON value serializes into a hasher");
}

struct HashWriter(Sha256);

impl std::io::Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
