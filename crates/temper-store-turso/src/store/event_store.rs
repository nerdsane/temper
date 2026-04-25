//! [`EventStore`] trait implementation for Turso/libSQL.

use libsql::{TransactionBehavior, params};
use std::time::{Duration, Instant};
use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceEnvelope, PersistenceError, storage_error,
};
use temper_runtime::tenant::parse_persistence_id_parts;
use tracing::instrument;

use super::TursoEventStore;
use super::instrumentation::record_turso_query_duration;
use crate::metrics::record_turso_write_retry;
use crate::retry::{
    RETRY_DELAYS_MS, is_transient_write_error, timeout_error, write_attempt_timeout,
};

impl EventStore for TursoEventStore {
    #[instrument(skip_all, fields(
        persistence_id,
        event_count = events.len(),
        append_path = tracing::field::Empty,
        otel.name = "turso.append",
    ))]
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        // Retry transient Hrana BLOCKED / stream errors with backoff (ADR-0056).
        // The whole function body is the retry unit: each attempt opens a fresh
        // transaction. Event-store's UNIQUE (entity_type, entity_id, sequence_nr)
        // makes retries safe — if a prior attempt partially committed before
        // erroring, the retry's pre-check detects it as ConcurrencyViolation
        // (non-transient, propagates to caller via normal event-store contract).
        let total_attempts = RETRY_DELAYS_MS.len() + 1;
        let mut last_err: Option<PersistenceError> = None;
        for attempt in 0..total_attempts {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(RETRY_DELAYS_MS[attempt - 1])).await;
            }
            let attempt_timeout = write_attempt_timeout();
            match tokio::time::timeout(
                attempt_timeout,
                self.append_inner(persistence_id, expected_sequence, events),
            )
            .await
            {
                Ok(Ok(seq)) => {
                    if attempt > 0 {
                        record_turso_write_retry("turso.append", attempt as u64, "succeeded");
                    }
                    return Ok(seq);
                }
                Ok(Err(err)) => {
                    let transient = match &err {
                        PersistenceError::Storage(msg) => is_transient_write_error(msg),
                        _ => false,
                    };
                    if !transient {
                        return Err(err);
                    }
                    last_err = Some(err);
                }
                Err(_) => {
                    last_err = Some(timeout_error("turso.append", attempt_timeout));
                }
            }
        }
        record_turso_write_retry("turso.append", total_attempts as u64, "exhausted");
        Err(last_err.expect("retry loop captured at least one error"))
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.read_events"))]
    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;

        let mut rows = conn
            .query(
                "SELECT sequence_nr, event_type, payload, metadata
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr > ?4
                 ORDER BY sequence_nr ASC",
                params![tenant, entity_type, entity_id, from_sequence as i64],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let seq = row.get::<i64>(0).map_err(storage_error)? as u64;
            let event_type = row.get::<String>(1).map_err(storage_error)?;
            let payload_json = row.get::<String>(2).map_err(storage_error)?;
            let metadata_json = row.get::<Option<String>>(3).map_err(storage_error)?;

            let payload = serde_json::from_str(&payload_json).map_err(|e| {
                tracing::error!(error = %e, "failed to deserialize event payload");
                PersistenceError::Serialization(e.to_string())
            })?;
            let metadata_raw = metadata_json.ok_or_else(|| {
                tracing::error!("missing event metadata");
                PersistenceError::Serialization("missing event metadata".to_string())
            })?;
            let metadata: EventMetadata = serde_json::from_str(&metadata_raw).map_err(|e| {
                tracing::error!(error = %e, "failed to deserialize event metadata");
                PersistenceError::Serialization(e.to_string())
            })?;

            out.push(PersistenceEnvelope {
                sequence_nr: seq,
                event_type,
                payload,
                metadata,
            });
        }

        Ok(out)
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.save_snapshot"))]
    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;

        conn.execute(
            "INSERT INTO snapshots (tenant, entity_type, entity_id, sequence_nr, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (tenant, entity_type, entity_id)
             DO UPDATE SET
                sequence_nr = excluded.sequence_nr,
                snapshot = excluded.snapshot,
                created_at = datetime('now')",
            params![
                tenant,
                entity_type,
                entity_id,
                sequence_nr as i64,
                snapshot.to_vec()
            ],
        )
        .await
        .map_err(storage_error)?;

        Ok(())
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.load_snapshot"))]
    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT sequence_nr, snapshot
                 FROM snapshots
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                 ORDER BY sequence_nr DESC
                 LIMIT 1",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;

        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };

        let sequence_nr = row.get::<i64>(0).map_err(storage_error)? as u64;
        let snapshot = row.get::<Vec<u8>>(1).map_err(storage_error)?;
        Ok(Some((sequence_nr, snapshot)))
    }

    #[instrument(skip_all, fields(tenant, otel.name = "turso.list_entity_ids"))]
    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT DISTINCT e.entity_type, e.entity_id
                 FROM events e
                 WHERE e.tenant = ?1
                   AND NOT EXISTS (
                     SELECT 1
                     FROM events d
                     WHERE d.tenant = e.tenant
                       AND d.entity_type = e.entity_type
                       AND d.entity_id = e.entity_id
                       AND d.event_type = 'Deleted'
                   )",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_type = row.get::<String>(0).map_err(storage_error)?;
            let entity_id = row.get::<String>(1).map_err(storage_error)?;
            out.push((entity_type, entity_id));
        }
        Ok(out)
    }
}

impl TursoEventStore {
    /// Single-attempt implementation of [`EventStore::append`]. Callers go
    /// through the public `append` which wraps this in retry-with-backoff
    /// (ADR-0056). Kept as an inherent `async fn` on the concrete type so the
    /// transactional body can borrow `self` cleanly across retries without
    /// fighting `FnMut` + future-lifetime rules.
    ///
    /// Safe to retry after a transient transport failure: the UNIQUE
    /// constraint on `events.(entity_type, entity_id, sequence_nr)` means a
    /// prior-attempt partial commit is detected as `ConcurrencyViolation`,
    /// which the retry layer treats as non-transient and propagates to the
    /// caller via the normal event-store contract.
    async fn append_inner(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;

        if events.len() == 1 {
            tracing::Span::current().record("append_path", "single_statement");
            return Self::append_one_fast_path(
                &conn,
                tenant,
                entity_type,
                entity_id,
                expected_sequence,
                &events[0],
            )
            .await;
        }
        tracing::Span::current().record("append_path", "explicit_transaction");

        let begin_start = Instant::now();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await;
        record_turso_query_duration(begin_start.elapsed(), "begin", "transaction", tx.is_ok());
        let tx = tx.map_err(storage_error)?;

        let select_start = Instant::now();
        let rows_result = tx
            .query(
                "SELECT COALESCE(MAX(sequence_nr), 0)
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await;
        record_turso_query_duration(
            select_start.elapsed(),
            "query",
            "transaction",
            rows_result.is_ok(),
        );
        let mut rows = rows_result.map_err(storage_error)?;

        let current_seq = match rows.next().await.map_err(storage_error)? {
            Some(row) => row.get::<i64>(0).map_err(storage_error)? as u64,
            None => 0,
        };
        drop(rows);

        if current_seq != expected_sequence {
            tracing::error!(
                expected = expected_sequence,
                actual = current_seq,
                "concurrency violation on append"
            );
            Self::rollback_with_metric(tx).await;
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: current_seq,
            });
        }

        let mut new_seq = expected_sequence;
        for event in events {
            new_seq += 1;
            let (payload_json, metadata_json) = Self::serialize_event(event)?;

            let insert_start = Instant::now();
            let insert_result = tx
                .execute(
                    "INSERT INTO events
                     (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        tenant,
                        entity_type,
                        entity_id,
                        new_seq as i64,
                        event.event_type.as_str(),
                        payload_json,
                        metadata_json
                    ],
                )
                .await;
            record_turso_query_duration(
                insert_start.elapsed(),
                "execute",
                "transaction",
                insert_result.is_ok(),
            );

            if let Err(e) = insert_result {
                let msg = e.to_string();
                tracing::error!(error = %e, "event insert failed");
                Self::rollback_with_metric(tx).await;
                if msg.contains("UNIQUE constraint failed") || msg.contains("UNIQUE") {
                    return Err(PersistenceError::ConcurrencyViolation {
                        expected: expected_sequence,
                        actual: new_seq,
                    });
                }
                return Err(PersistenceError::Storage(msg));
            }
        }

        let commit_start = Instant::now();
        let commit_result = tx.commit().await;
        record_turso_query_duration(
            commit_start.elapsed(),
            "commit",
            "transaction",
            commit_result.is_ok(),
        );
        commit_result.map_err(storage_error)?;
        Ok(new_seq)
    }

    async fn append_one_fast_path(
        conn: &super::instrumentation::InstrumentedConnection,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        expected_sequence: u64,
        event: &PersistenceEnvelope,
    ) -> Result<u64, PersistenceError> {
        let new_seq = expected_sequence + 1;
        let (payload_json, metadata_json) = Self::serialize_event(event)?;
        let insert_result = conn
            .execute(
                "INSERT INTO events
                 (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
                 WHERE (
                     SELECT COALESCE(MAX(sequence_nr), 0)
                     FROM events
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                 ) = ?8",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    new_seq as i64,
                    event.event_type.as_str(),
                    payload_json,
                    metadata_json,
                    expected_sequence as i64,
                ],
            )
            .await;

        match insert_result {
            Ok(1) => Ok(new_seq),
            Ok(0) => {
                let actual = Self::current_sequence(conn, tenant, entity_type, entity_id).await?;
                tracing::error!(
                    expected = expected_sequence,
                    actual,
                    "concurrency violation on append"
                );
                Err(PersistenceError::ConcurrencyViolation {
                    expected: expected_sequence,
                    actual,
                })
            }
            Ok(rows) => Err(PersistenceError::Storage(format!(
                "single-event append inserted unexpected row count {rows}"
            ))),
            Err(e) => {
                let msg = e.to_string();
                tracing::error!(error = %e, "event insert failed");
                if msg.contains("UNIQUE constraint failed") || msg.contains("UNIQUE") {
                    let actual = Self::current_sequence(conn, tenant, entity_type, entity_id)
                        .await
                        .unwrap_or(new_seq);
                    return Err(PersistenceError::ConcurrencyViolation {
                        expected: expected_sequence,
                        actual,
                    });
                }
                Err(PersistenceError::Storage(msg))
            }
        }
    }

    async fn current_sequence(
        conn: &super::instrumentation::InstrumentedConnection,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<u64, PersistenceError> {
        let mut rows = conn
            .query(
                "SELECT COALESCE(MAX(sequence_nr), 0)
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
        match rows.next().await.map_err(storage_error)? {
            Some(row) => Ok(row.get::<i64>(0).map_err(storage_error)? as u64),
            None => Ok(0),
        }
    }

    fn serialize_event(event: &PersistenceEnvelope) -> Result<(String, String), PersistenceError> {
        let payload_json = serde_json::to_string(&event.payload).map_err(|e| {
            tracing::error!(error = %e, "failed to serialize event payload");
            PersistenceError::Serialization(e.to_string())
        })?;
        let metadata_json = serde_json::to_string(&event.metadata).map_err(|e| {
            tracing::error!(error = %e, "failed to serialize event metadata");
            PersistenceError::Serialization(e.to_string())
        })?;
        Ok((payload_json, metadata_json))
    }

    async fn rollback_with_metric(tx: libsql::Transaction) {
        let rollback_start = Instant::now();
        let rollback_result = tx.rollback().await;
        record_turso_query_duration(
            rollback_start.elapsed(),
            "rollback",
            "transaction",
            rollback_result.is_ok(),
        );
    }
}
