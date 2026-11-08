//! A resumable migration that keeps who made each record in its catalog row,
//! from the provenance of its event at sequence one, as the kernel now keeps it
//! on every new record. Batches hold the write gate as briefly as the creation
//! time migration's ([`super::first_event`]). Rows whose first event is no
//! longer held, or predates provenance, stay unknown.
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{TursoEventStore, WritePriority};
use crate::driver::params;
use temper_runtime::persistence::{PersistenceError, storage_error};

pub(super) const CREATE_BACKFILL_TABLE: &str = "CREATE TABLE IF NOT EXISTS first_origin_backfill (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    tenant TEXT NOT NULL DEFAULT '', entity_type TEXT NOT NULL DEFAULT '', entity_id TEXT NOT NULL DEFAULT '',
    records INTEGER NOT NULL DEFAULT 0, unknown_records INTEGER NOT NULL DEFAULT 0,
    complete INTEGER NOT NULL DEFAULT 0
)";
/// The most rows one batch visits, however small they are.
const BATCH_SIZE: usize = 128;
/// The event's provenance keys and the row keys they are kept under.
const ORIGIN_KEYS: [(&str, &str); 5] = [
    ("via", "first_event_via"),
    ("by", "first_event_by"),
    ("for", "first_event_for"),
    ("agent_type", "first_event_agent_type"),
    ("label", "first_event_label"),
];

impl TursoEventStore {
    pub(super) async fn start_first_origin_backfill(&self) -> Result<(), PersistenceError> {
        let conn = self
            .write_connection("first_origin_backfill_start", WritePriority::Low)
            .await?;
        // As in the creation time migration: probe, then initialize, on a
        // connection that holds the write gate, closing reads before writes.
        let cursor_exists = {
            let mut rows = conn
                .query("SELECT 1 FROM first_origin_backfill WHERE id=1", ())
                .await
                .map_err(storage_error)?;
            rows.next().await.map_err(storage_error)?.is_some()
        };
        if !cursor_exists {
            conn.execute("INSERT INTO first_origin_backfill (id, complete) SELECT 1, NOT EXISTS (SELECT 1 FROM entity_catalog LIMIT 1) ON CONFLICT(id) DO NOTHING", ()).await.map_err(storage_error)?;
        }
        let mut rows = conn
            .query("SELECT complete FROM first_origin_backfill WHERE id=1", ())
            .await
            .map_err(storage_error)?;
        let complete = rows
            .next()
            .await
            .map_err(storage_error)?
            .ok_or_else(|| PersistenceError::Storage("missing maker backfill cursor".into()))?
            .get::<i64>(0)
            .map_err(storage_error)?;
        if complete != 0 {
            return Ok(());
        }
        drop(rows);
        drop(conn);
        let store = self.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            loop {
                match store
                    .backfill_first_origin_batch(super::first_event::BATCH_HOLD)
                    .await
                {
                    Ok(Some((records, unknown_records))) => {
                        tracing::info!(
                            records,
                            unknown_records,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "first_event_by backfill complete"
                        );
                        return;
                    }
                    Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(error) => {
                        tracing::error!(%error, "first_event_by backfill stopped; resumes on next start");
                        return;
                    }
                }
            }
        });
        Ok(())
    }

    /// The records after the cursor, in key order, committed with the cursor
    /// once the batch has held the write gate for `hold` or visited
    /// [`BATCH_SIZE`] rows. A row already naming its maker is left as it is.
    pub(super) async fn backfill_first_origin_batch(
        &self,
        hold: Duration,
    ) -> Result<Option<(u64, u64)>, PersistenceError> {
        let conn = self
            .write_connection("first_origin_backfill", WritePriority::Low)
            .await?;
        let held = Instant::now();
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        tx.execute(
            "INSERT INTO first_origin_backfill (id) VALUES (1) ON CONFLICT(id) DO NOTHING",
            (),
        )
        .await
        .map_err(storage_error)?;
        let mut progress = tx.query("SELECT tenant, entity_type, entity_id, records, unknown_records, complete FROM first_origin_backfill WHERE id = 1", ()).await.map_err(storage_error)?;
        let progress = progress
            .next()
            .await
            .map_err(storage_error)?
            .ok_or_else(|| PersistenceError::Storage("missing maker backfill cursor".into()))?;
        let mut cursor: (String, String, String) = (
            progress.get(0).map_err(storage_error)?,
            progress.get(1).map_err(storage_error)?,
            progress.get(2).map_err(storage_error)?,
        );
        let mut records: i64 = progress.get(3).map_err(storage_error)?;
        let mut unknown_records: i64 = progress.get(4).map_err(storage_error)?;
        let complete: i64 = progress.get(5).map_err(storage_error)?;
        if complete != 0 {
            tx.commit().await.map_err(storage_error)?;
            return Ok(Some((records as u64, unknown_records as u64)));
        }
        let mut done = false;
        for _ in 0..BATCH_SIZE {
            // Both sides use their (tenant, type, id[, sequence]) index. The
            // read closes before its row is written.
            let (key, state, payload) = {
                let mut rows = tx.query(
                    "SELECT c.tenant, c.entity_type, c.entity_id, c.state, e.payload
                     FROM entity_catalog c INDEXED BY sqlite_autoindex_entity_catalog_1
                     LEFT JOIN events e INDEXED BY idx_events_entity
                       ON e.tenant=c.tenant AND e.entity_type=c.entity_type AND e.entity_id=c.entity_id AND e.sequence_nr=1
                     WHERE (c.tenant, c.entity_type, c.entity_id) > (?1, ?2, ?3)
                     ORDER BY c.tenant, c.entity_type, c.entity_id LIMIT 1",
                    params![cursor.0.as_str(), cursor.1.as_str(), cursor.2.as_str()]).await.map_err(storage_error)?;
                let Some(row) = rows.next().await.map_err(storage_error)? else {
                    done = true;
                    break;
                };
                let key = (
                    row.get::<String>(0).map_err(storage_error)?,
                    row.get::<String>(1).map_err(storage_error)?,
                    row.get::<String>(2).map_err(storage_error)?,
                );
                let state: Option<String> = row.get(3).map_err(storage_error)?;
                let payload: Option<String> = row.get(4).map_err(storage_error)?;
                (key, state, payload)
            };
            let kept = state
                .as_deref()
                .map(|state| with_first_origin(state, payload.as_deref()))
                .transpose()?;
            let (tenant, entity_type, entity_id) = &key;
            match kept {
                Some(Kept::Written(state)) => {
                    tx.execute("UPDATE entity_catalog INDEXED BY sqlite_autoindex_entity_catalog_1 SET state = ?4 WHERE tenant=?1 AND entity_type=?2 AND entity_id=?3",
                        params![tenant.as_str(), entity_type.as_str(), entity_id.as_str(), state]).await.map_err(storage_error)?;
                }
                Some(Kept::Already) => {}
                Some(Kept::Unknown) | None => unknown_records += 1,
            }
            records += 1;
            cursor = key;
            if held.elapsed() >= hold {
                break;
            }
        }
        let (tenant, entity_type, entity_id) = cursor;
        tx.execute("UPDATE first_origin_backfill SET tenant=?1, entity_type=?2, entity_id=?3, records=?4, unknown_records=?5, complete=?6 WHERE id=1",
            params![tenant, entity_type, entity_id, records, unknown_records, i64::from(done)]).await.map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(done.then_some((records as u64, unknown_records as u64)))
    }
}

/// What one catalog row needs.
#[derive(Debug, PartialEq)]
enum Kept {
    /// The row already names its maker.
    Already,
    /// Its creation is no longer held or predates provenance.
    Unknown,
    /// The row's state with its maker added.
    Written(String),
}

/// The stored `state` with the provenance of the creation event `payload`
/// laid in at the top level, the shape the kernel writes.
fn with_first_origin(state: &str, payload: Option<&str>) -> Result<Kept, PersistenceError> {
    let serialization =
        |error: serde_json::Error| PersistenceError::Serialization(error.to_string());
    let mut state: Value = serde_json::from_str(state).map_err(serialization)?;
    let Some(row) = state.as_object_mut() else {
        return Ok(Kept::Unknown);
    };
    if row.get("first_event_via").is_some_and(|via| !via.is_null()) {
        return Ok(Kept::Already);
    }
    let Some(payload) = payload else {
        return Ok(Kept::Unknown);
    };
    let event: Value = serde_json::from_str(payload).map_err(serialization)?;
    if event.get("via").is_none_or(Value::is_null) {
        return Ok(Kept::Unknown);
    }
    for (from, to) in ORIGIN_KEYS {
        if let Some(value) = event.get(from).filter(|value| !value.is_null()) {
            row.insert(to.to_string(), value.clone());
        }
    }
    Ok(Kept::Written(
        serde_json::to_string(&state).map_err(serialization)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_takes_its_maker_from_the_creation_event() {
        let kept = with_first_origin(
            r#"{"status":"Open","last_event_by":"bo"}"#,
            Some(r#"{"action":"Create","via":"request","by":"ann","for":"alice","agent_type":"claude","label":"five-miles","role":"member"}"#),
        )
        .unwrap();
        let Kept::Written(state) = kept else {
            panic!("expected a write, got {kept:?}");
        };
        let state: Value = serde_json::from_str(&state).unwrap();
        assert_eq!(state["first_event_via"], "request");
        assert_eq!(state["first_event_by"], "ann");
        assert_eq!(state["first_event_for"], "alice");
        assert_eq!(state["first_event_agent_type"], "claude");
        assert_eq!(state["first_event_label"], "five-miles");
        assert_eq!(state["last_event_by"], "bo");
        assert!(state.get("first_event_role").is_none());
    }

    #[test]
    fn a_row_without_a_known_creation_stays_unknown() {
        let row = r#"{"status":"Open"}"#;
        assert_eq!(with_first_origin(row, None).unwrap(), Kept::Unknown);
        assert_eq!(
            with_first_origin(row, Some(r#"{"action":"Create","by":"ann"}"#)).unwrap(),
            Kept::Unknown
        );
        assert_eq!(
            with_first_origin(
                r#"{"first_event_via":"timer"}"#,
                Some(r#"{"via":"request","by":"ann"}"#)
            )
            .unwrap(),
            Kept::Already
        );
    }
}
