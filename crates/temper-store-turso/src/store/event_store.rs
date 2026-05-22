//! [`EventStore`] trait implementation for Turso/libSQL.

use libsql::{TransactionBehavior, Value, params, params_from_iter};
use std::time::Duration;
use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
    PersistenceError, storage_error,
};
use temper_runtime::tenant::parse_persistence_id_parts;
use tracing::{instrument, warn};

use super::TursoEventStore;
use super::append_config::{append_attempt_timeout, append_max_attempts};
use super::instrumentation::record_turso_query_duration;
use super::write_gate::WritePriority;
use crate::metrics::record_turso_write_retry;
use crate::retry::{is_transient_write_error, retry_delay_ms};

const APPEND_BATCH_INSERT_CHUNK_ROWS: usize = 400;

struct PreparedEventInsert {
    tenant: String,
    entity_type: String,
    entity_id: String,
    sequence_nr: u64,
    event_type: String,
    payload_json: String,
    metadata_json: String,
    expected_sequence: u64,
}

impl EventStore for TursoEventStore {
    #[instrument(skip_all, fields(persistence_id, otel.name = "turso.append"))]
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        if events.is_empty() {
            return Ok(expected_sequence);
        }

        // Retry transient Hrana BLOCKED / stream errors with backoff (ADR-0056).
        // Each attempt is a complete append unit. Single-event appends use an
        // atomic conditional insert; multi-event appends open a transaction.
        // Event-store's UNIQUE (entity_type, entity_id, sequence_nr) makes
        // retries safe — if a prior attempt partially committed before erroring,
        // the retry's pre-check detects it as ConcurrencyViolation
        // (non-transient, propagates to caller via normal event-store contract).
        let attempt_timeout = append_attempt_timeout();
        let total_attempts = append_max_attempts();
        let mut last_err: Option<PersistenceError> = None;
        let bypass_write_gate = events.len() == 1;
        for attempt in 0..total_attempts {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(retry_delay_ms(attempt - 1))).await;
            }
            let _high_priority_marker = if bypass_write_gate {
                Some(self.mark_high_priority_write("turso.append"))
            } else {
                None
            };
            let _write_permit = if bypass_write_gate {
                None
            } else {
                Some(
                    self.acquire_write_permit("turso.append", WritePriority::High)
                        .await?,
                )
            };
            let attempt_result = tokio::time::timeout(
                attempt_timeout,
                self.append_inner(persistence_id, expected_sequence, events),
            )
            .await
            .unwrap_or_else(|_| {
                warn!(
                    persistence_id,
                    attempt,
                    timeout_ms = attempt_timeout.as_millis() as u64,
                    "turso.append attempt timed out"
                );
                Err(PersistenceError::Storage(format!(
                    "turso.append timed out after {}ms",
                    attempt_timeout.as_millis()
                )))
            });

            match attempt_result {
                Ok(seq) => {
                    if attempt > 0 {
                        record_turso_write_retry("turso.append", attempt as u64, "succeeded");
                    }
                    return Ok(seq);
                }
                Err(err) => {
                    let transient = match &err {
                        PersistenceError::Storage(msg) => is_transient_write_error(msg),
                        _ => false,
                    };
                    if !transient {
                        return Err(err);
                    }
                    last_err = Some(err);
                }
            }
        }
        record_turso_write_retry("turso.append", total_attempts as u64, "exhausted");
        Err(last_err.expect("retry loop captured at least one error"))
    }

    #[instrument(skip_all, fields(otel.name = "turso.append_batch"))]
    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        if appends.is_empty() {
            return Ok(Vec::new());
        }
        if let [append] = appends {
            let sequence_nr = self
                .append(
                    &append.persistence_id,
                    append.expected_sequence,
                    &append.events,
                )
                .await?;
            return Ok(vec![PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr,
            }]);
        }

        let attempt_timeout = append_attempt_timeout();
        let total_attempts = append_max_attempts();
        let mut last_err: Option<PersistenceError> = None;
        for attempt in 0..total_attempts {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(retry_delay_ms(attempt - 1))).await;
            }
            let _write_permit = self
                .acquire_write_permit("turso.append_batch", WritePriority::High)
                .await?;
            let attempt_result =
                tokio::time::timeout(attempt_timeout, self.append_batch_inner(appends))
                    .await
                    .unwrap_or_else(|_| {
                        warn!(
                            attempt,
                            timeout_ms = attempt_timeout.as_millis() as u64,
                            "turso.append_batch attempt timed out"
                        );
                        Err(PersistenceError::Storage(format!(
                            "turso.append_batch timed out after {}ms",
                            attempt_timeout.as_millis()
                        )))
                    });

            match attempt_result {
                Ok(result) => {
                    if attempt > 0 {
                        record_turso_write_retry("turso.append_batch", attempt as u64, "succeeded");
                    }
                    return Ok(result);
                }
                Err(err) => {
                    let transient = matches!(&err, PersistenceError::Storage(msg) if is_transient_write_error(msg));
                    if !transient {
                        return Err(err);
                    }
                    last_err = Some(err);
                }
            }
        }
        record_turso_write_retry("turso.append_batch", total_attempts as u64, "exhausted");
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
        let _write_permit = self
            .acquire_write_permit("turso.save_snapshot", WritePriority::Low)
            .await?;
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

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.list_entity_ids_by_type_from_read_sources(tenant, entity_type)
            .await
    }

    async fn list_entity_ids_limited(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.configured_connection().await?;
        let limit = limit.min(i64::MAX as usize) as i64;
        let mut out = Vec::new();

        if let Some(entity_type) = entity_type {
            let mut rows = conn
                .query(
                    "SELECT DISTINCT e.entity_type, e.entity_id
                     FROM events e
                     WHERE e.tenant = ?1
                       AND e.entity_type = ?2
                       AND NOT EXISTS (
                         SELECT 1
                         FROM events d
                         WHERE d.tenant = e.tenant
                           AND d.entity_type = e.entity_type
                           AND d.entity_id = e.entity_id
                           AND d.event_type = 'Deleted'
                       )
                     ORDER BY e.entity_type, e.entity_id
                     LIMIT ?3",
                    params![tenant, entity_type, limit],
                )
                .await
                .map_err(storage_error)?;

            while let Some(row) = rows.next().await.map_err(storage_error)? {
                out.push((
                    row.get::<String>(0).map_err(storage_error)?,
                    row.get::<String>(1).map_err(storage_error)?,
                ));
            }
            return Ok(out);
        }

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
                   )
                 ORDER BY e.entity_type, e.entity_id
                 LIMIT ?2",
                params![tenant, limit],
            )
            .await
            .map_err(storage_error)?;

        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push((
                row.get::<String>(0).map_err(storage_error)?,
                row.get::<String>(1).map_err(storage_error)?,
            ));
        }
        Ok(out)
    }
}

impl TursoEventStore {
    /// List tenants with at least one persisted event.
    #[instrument(skip_all, fields(otel.name = "turso.list_event_tenants"))]
    pub async fn list_event_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query("SELECT DISTINCT tenant FROM events ORDER BY tenant", ())
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(out)
    }

    /// List tenants appearing in any tenant-scoped storage table.
    #[instrument(skip_all, fields(otel.name = "turso.list_storage_tenants"))]
    pub async fn list_storage_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant FROM events \
                 UNION SELECT tenant FROM specs \
                 UNION SELECT tenant FROM trajectories \
                 UNION SELECT tenant FROM tenant_constraints \
                 UNION SELECT tenant FROM wasm_modules \
                 UNION SELECT tenant FROM wasm_invocation_logs \
                 UNION SELECT tenant FROM pending_decisions \
                 UNION SELECT tenant FROM tenant_policies \
                 UNION SELECT tenant FROM policies \
                 UNION SELECT tenant_id AS tenant FROM tenant_installed_apps \
                 UNION SELECT tenant FROM policy_denial_patterns \
                 UNION SELECT tenant FROM tenant_secrets \
                 UNION SELECT tenant FROM design_time_events \
                 UNION SELECT tenant FROM ots_trajectories \
                 UNION SELECT tenant FROM entity_catalog \
                 ORDER BY tenant",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let tenant = row.get::<String>(0).map_err(storage_error)?;
            if !tenant.trim().is_empty() {
                out.push(tenant);
            }
        }
        Ok(out)
    }

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
        if events.is_empty() {
            return Ok(expected_sequence);
        }

        if let [event] = events {
            return self
                .append_single_event_inner(persistence_id, expected_sequence, event)
                .await;
        }

        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        let select_start = std::time::Instant::now();
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
            let _ = tx.rollback().await;
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: current_seq,
            });
        }

        let mut new_seq = expected_sequence;
        for event in events {
            new_seq += 1;
            let payload_json = serde_json::to_string(&event.payload).map_err(|e| {
                tracing::error!(error = %e, "failed to serialize event payload");
                PersistenceError::Serialization(e.to_string())
            })?;
            let metadata_json = serde_json::to_string(&event.metadata).map_err(|e| {
                tracing::error!(error = %e, "failed to serialize event metadata");
                PersistenceError::Serialization(e.to_string())
            })?;

            let insert_start = std::time::Instant::now();
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
                let _ = tx.rollback().await;
                if msg.contains("UNIQUE constraint failed") || msg.contains("UNIQUE") {
                    return Err(PersistenceError::ConcurrencyViolation {
                        expected: expected_sequence,
                        actual: new_seq,
                    });
                }
                return Err(PersistenceError::Storage(msg));
            }
        }

        tx.commit().await.map_err(storage_error)?;
        Ok(new_seq)
    }

    async fn append_batch_inner(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        let mut seen = std::collections::BTreeSet::new();
        for append in appends {
            if !seen.insert(append.persistence_id.as_str()) {
                return Err(PersistenceError::Storage(format!(
                    "duplicate persistence_id '{}' in append_batch",
                    append.persistence_id
                )));
            }
        }

        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        let mut parsed = Vec::with_capacity(appends.len());
        for append in appends {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;

            if append.expected_sequence == 0 && !append.events.is_empty() {
                parsed.push((
                    tenant.to_string(),
                    entity_type.to_string(),
                    entity_id.to_string(),
                ));
                continue;
            }

            let select_start = std::time::Instant::now();
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

            if current_seq != append.expected_sequence {
                tracing::error!(
                    expected = append.expected_sequence,
                    actual = current_seq,
                    persistence_id = %append.persistence_id,
                    "concurrency violation on append_batch"
                );
                let _ = tx.rollback().await;
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: append.expected_sequence,
                    actual: current_seq,
                });
            }
            parsed.push((
                tenant.to_string(),
                entity_type.to_string(),
                entity_id.to_string(),
            ));
        }

        let mut results = Vec::with_capacity(appends.len());
        let mut event_rows = Vec::new();
        for (append, (tenant, entity_type, entity_id)) in appends.iter().zip(parsed.iter()) {
            let mut new_seq = append.expected_sequence;
            for event in &append.events {
                new_seq += 1;
                let payload_json = serde_json::to_string(&event.payload).map_err(|e| {
                    tracing::error!(error = %e, "failed to serialize event payload");
                    PersistenceError::Serialization(e.to_string())
                })?;
                let metadata_json = serde_json::to_string(&event.metadata).map_err(|e| {
                    tracing::error!(error = %e, "failed to serialize event metadata");
                    PersistenceError::Serialization(e.to_string())
                })?;

                event_rows.push(PreparedEventInsert {
                    tenant: tenant.clone(),
                    entity_type: entity_type.clone(),
                    entity_id: entity_id.clone(),
                    sequence_nr: new_seq,
                    event_type: event.event_type.clone(),
                    payload_json,
                    metadata_json,
                    expected_sequence: append.expected_sequence,
                });
            }
            results.push(PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr: new_seq,
            });
        }

        for chunk in event_rows.chunks(APPEND_BATCH_INSERT_CHUNK_ROWS) {
            if chunk.is_empty() {
                continue;
            }

            let mut insert_sql = String::from(
                "INSERT INTO events \
                 (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata) \
                 VALUES ",
            );
            let mut insert_values = Vec::with_capacity(chunk.len() * 7);
            for (index, row) in chunk.iter().enumerate() {
                if index > 0 {
                    insert_sql.push_str(", ");
                }
                insert_sql.push_str("(?, ?, ?, ?, ?, ?, ?)");
                insert_values.push(Value::from(row.tenant.clone()));
                insert_values.push(Value::from(row.entity_type.clone()));
                insert_values.push(Value::from(row.entity_id.clone()));
                insert_values.push(Value::from(row.sequence_nr as i64));
                insert_values.push(Value::from(row.event_type.clone()));
                insert_values.push(Value::from(row.payload_json.clone()));
                insert_values.push(Value::from(row.metadata_json.clone()));
            }

            let insert_start = std::time::Instant::now();
            let insert_result = tx
                .execute(&insert_sql, params_from_iter(insert_values))
                .await;
            record_turso_query_duration(
                insert_start.elapsed(),
                "execute",
                "transaction",
                insert_result.is_ok(),
            );

            if let Err(e) = insert_result {
                let msg = e.to_string();
                tracing::error!(error = %e, "event batch insert failed");
                let _ = tx.rollback().await;
                if msg.contains("UNIQUE constraint failed") || msg.contains("UNIQUE") {
                    let first = &chunk[0];
                    return Err(PersistenceError::ConcurrencyViolation {
                        expected: first.expected_sequence,
                        actual: first.sequence_nr,
                    });
                }
                return Err(PersistenceError::Storage(msg));
            }
        }

        tx.commit().await.map_err(storage_error)?;
        Ok(results)
    }

    /// Atomic fast path for the common event-store case: one entity action
    /// produces one event. On remote Turso this avoids holding an explicit
    /// Hrana transaction across BEGIN/SELECT/INSERT/COMMIT round trips.
    async fn append_single_event_inner(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        event: &PersistenceEnvelope,
    ) -> Result<u64, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let new_seq = expected_sequence + 1;
        let payload_json = serde_json::to_string(&event.payload).map_err(|e| {
            tracing::error!(error = %e, "failed to serialize event payload");
            PersistenceError::Serialization(e.to_string())
        })?;
        let metadata_json = serde_json::to_string(&event.metadata).map_err(|e| {
            tracing::error!(error = %e, "failed to serialize event metadata");
            PersistenceError::Serialization(e.to_string())
        })?;

        let conn = self.configured_connection().await?;
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
                    expected_sequence as i64
                ],
            )
            .await;

        let affected = match insert_result {
            Ok(affected) => affected,
            Err(e) => {
                let msg = e.to_string();
                tracing::error!(error = %e, "single event insert failed");
                if msg.contains("UNIQUE constraint failed") || msg.contains("UNIQUE") {
                    let actual = current_sequence(&conn, tenant, entity_type, entity_id).await?;
                    return Err(PersistenceError::ConcurrencyViolation {
                        expected: expected_sequence,
                        actual,
                    });
                }
                return Err(PersistenceError::Storage(msg));
            }
        };

        if affected == 1 {
            return Ok(new_seq);
        }

        let actual = current_sequence(&conn, tenant, entity_type, entity_id).await?;
        tracing::error!(
            expected = expected_sequence,
            actual,
            affected,
            "concurrency violation on single event append"
        );
        Err(PersistenceError::ConcurrencyViolation {
            expected: expected_sequence,
            actual,
        })
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
        Some(row) => row
            .get::<i64>(0)
            .map_err(storage_error)
            .map(|seq| seq as u64),
        None => Ok(0),
    }
}
