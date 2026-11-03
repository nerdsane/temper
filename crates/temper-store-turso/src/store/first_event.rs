//! A resumable creation-time migration in batches that hold the write gate
//! briefly. No record walk delays open.
use std::time::{Duration, Instant};

use super::{TursoEventStore, WritePriority};
use crate::driver::params;
use temper_runtime::persistence::{PersistenceError, storage_error};

pub(super) const CREATE_BACKFILL_TABLE: &str = "CREATE TABLE IF NOT EXISTS first_event_backfill (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    tenant TEXT NOT NULL DEFAULT '', entity_type TEXT NOT NULL DEFAULT '', entity_id TEXT NOT NULL DEFAULT '',
    records INTEGER NOT NULL DEFAULT 0, null_records INTEGER NOT NULL DEFAULT 0,
    complete INTEGER NOT NULL DEFAULT 0
)";
/// How long one batch may hold the write gate. A boot write or a move that
/// arrives during a batch waits for its commit, so this is what the back-fill
/// adds to one: double-digit milliseconds, the speed asked of a small change.
/// A batch stops by time, not by count, because a row costs its size: 128
/// rows of about 240 KB held the gate 0.8 to 1.7 s per batch, and a first
/// start's five boot writes each waited for one (staging, 31 Oct 2026). Its
/// last row can take a batch past this; each batch writes at least one row,
/// so the back-fill always moves on.
pub(super) const BATCH_HOLD: Duration = Duration::from_millis(50);
/// The most rows one batch writes, however small they are.
const BATCH_SIZE: usize = 128;

impl TursoEventStore {
    pub(super) async fn start_first_event_backfill(&self) -> Result<(), PersistenceError> {
        // New databases and completed migrations need no background work. This
        // constant-size probe also keeps ordinary store users free of idle jobs.
        let conn = self
            .write_connection("first_event_backfill_start", WritePriority::Low)
            .await?;
        // Even a conflicting INSERT appends a Turso Sync transaction marker.
        // Hold the write gate across the probe and initialization, and close
        // the read cursor before writing on this connection.
        let cursor_exists = {
            let mut rows = conn
                .query("SELECT 1 FROM first_event_backfill WHERE id=1", ())
                .await
                .map_err(storage_error)?;
            rows.next().await.map_err(storage_error)?.is_some()
        };
        if !cursor_exists {
            conn.execute("INSERT INTO first_event_backfill (id, complete) SELECT 1, NOT EXISTS (SELECT 1 FROM entity_catalog LIMIT 1) ON CONFLICT(id) DO NOTHING", ()).await.map_err(storage_error)?;
        }
        let mut rows = conn
            .query("SELECT complete FROM first_event_backfill WHERE id=1", ())
            .await
            .map_err(storage_error)?;
        let complete = rows
            .next()
            .await
            .map_err(storage_error)?
            .ok_or_else(|| PersistenceError::Storage("missing creation backfill cursor".into()))?
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
                match store.backfill_first_event_batch(BATCH_HOLD).await {
                    Ok(Some((records, null_records))) => {
                        tracing::info!(
                            records,
                            null_records,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "first_event_at backfill complete"
                        );
                        return;
                    }
                    Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(error) => {
                        tracing::error!(%error, "first_event_at backfill stopped; resumes on next start");
                        return;
                    }
                }
            }
        });
        Ok(())
    }

    /// The records after the cursor, in key order, one at a time, committed
    /// with the cursor once the batch has held the write gate for `hold` or
    /// written [`BATCH_SIZE`] rows. Nulls are visited once too.
    pub(super) async fn backfill_first_event_batch(
        &self,
        hold: Duration,
    ) -> Result<Option<(u64, u64)>, PersistenceError> {
        let conn = self
            .write_connection("first_event_backfill", WritePriority::Low)
            .await?;
        let held = Instant::now();
        let tx = conn.begin_immediate().await.map_err(storage_error)?;
        tx.execute(
            "INSERT INTO first_event_backfill (id) VALUES (1) ON CONFLICT(id) DO NOTHING",
            (),
        )
        .await
        .map_err(storage_error)?;
        let mut progress = tx.query("SELECT tenant, entity_type, entity_id, records, null_records, complete FROM first_event_backfill WHERE id = 1", ()).await.map_err(storage_error)?;
        let progress = progress
            .next()
            .await
            .map_err(storage_error)?
            .ok_or_else(|| PersistenceError::Storage("missing creation backfill cursor".into()))?;
        let mut cursor: (String, String, String) = (
            progress.get(0).map_err(storage_error)?,
            progress.get(1).map_err(storage_error)?,
            progress.get(2).map_err(storage_error)?,
        );
        let mut records: i64 = progress.get(3).map_err(storage_error)?;
        let mut null_records: i64 = progress.get(4).map_err(storage_error)?;
        let complete: i64 = progress.get(5).map_err(storage_error)?;
        if complete != 0 {
            tx.commit().await.map_err(storage_error)?;
            return Ok(Some((records as u64, null_records as u64)));
        }
        let mut done = false;
        for _ in 0..BATCH_SIZE {
            // Both sides use their (tenant, type, id[, sequence]) index.
            // Sequence one is the first committed event, including in a
            // segmented journal. The read closes before its row is written.
            let (key, timestamp) = {
                let mut rows = tx.query(
                    "SELECT c.tenant, c.entity_type, c.entity_id, c.first_event_at,
                            COALESCE(json_extract(e.payload, '$.timestamp'), json_extract(e.metadata, '$.timestamp'))
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
                let existing: Option<String> = row.get(3).map_err(storage_error)?;
                let timestamp: Option<String> = row.get(4).map_err(storage_error)?;
                let timestamp = existing
                    .or(timestamp)
                    .map(|timestamp| {
                        chrono::DateTime::parse_from_rfc3339(&timestamp)
                            .map(|t| {
                                t.with_timezone(&chrono::Utc)
                                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                            })
                            .map_err(|error| PersistenceError::Serialization(error.to_string()))
                    })
                    .transpose()?;
                (key, timestamp)
            };
            let (tenant, entity_type, entity_id) = &key;
            tx.execute("UPDATE entity_catalog INDEXED BY sqlite_autoindex_entity_catalog_1 SET first_event_at = ?4 WHERE tenant=?1 AND entity_type=?2 AND entity_id=?3 AND first_event_at IS NULL",
                params![tenant.as_str(), entity_type.as_str(), entity_id.as_str(), timestamp.clone()]).await.map_err(storage_error)?;
            records += 1;
            null_records += i64::from(timestamp.is_none());
            cursor = key;
            if held.elapsed() >= hold {
                break;
            }
        }
        let (tenant, entity_type, entity_id) = cursor;
        tx.execute("UPDATE first_event_backfill SET tenant=?1, entity_type=?2, entity_id=?3, records=?4, null_records=?5, complete=?6 WHERE id=1",
            params![tenant, entity_type, entity_id, records, null_records, i64::from(done)]).await.map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(done.then_some((records as u64, null_records as u64)))
    }
}
