//! [`EventStore`] trait implementation for Turso/libSQL.

use libsql::{TransactionBehavior, Value, params, params_from_iter};
use std::time::Duration;
use temper_runtime::persistence::{
    EntityVectorCandidate, EntityVectorRow, EventMetadata, EventStore, PersistenceAppend,
    PersistenceAppendResult, PersistenceEnvelope, PersistenceError, PersistenceSequenceGuard,
    pack_f32_le, storage_error, unpack_f32_le, validate_guarded_persistence_append_batch,
};
use temper_runtime::tenant::parse_persistence_id_parts;
use tracing::{error, instrument, warn};

use super::TursoEventStore;
use super::append_config::{append_attempt_timeout, append_max_attempts};
use super::instrumentation::record_turso_query_duration;
use super::write_gate::WritePriority;
use crate::metrics::record_turso_write_retry;
use crate::retry::{is_transient_write_error, retry_delay_ms};

const APPEND_BATCH_INSERT_CHUNK_ROWS: usize = 400;

fn sqlite_sequence(sequence: u64, operation: &str) -> Result<i64, PersistenceError> {
    i64::try_from(sequence).map_err(|_| {
        PersistenceError::Storage(format!(
            "event sequence exceeds libSQL range during {operation}"
        ))
    })
}

struct PreparedEventInsert {
    tenant: String,
    entity_type: String,
    entity_id: String,
    sequence_nr: u64,
    stored_sequence_nr: i64,
    segment_index: i64,
    event_type: String,
    payload_json: String,
    metadata_json: String,
    expected_sequence: u64,
}

async fn open_segment_for_append(
    tx: &libsql::Transaction,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    current_sequence: u64,
) -> Result<i64, PersistenceError> {
    let mut segment_rows = tx
        .query(
            "SELECT segment_index, start_sequence_nr
             FROM event_segments
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sealed_at IS NULL
             ORDER BY segment_index DESC",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
    let open_segment = match segment_rows.next().await.map_err(storage_error)? {
        Some(row) => Some((
            row.get::<i64>(0).map_err(storage_error)?,
            row.get::<i64>(1).map_err(storage_error)?,
        )),
        None => None,
    };
    if segment_rows.next().await.map_err(storage_error)?.is_some() {
        return Err(PersistenceError::Storage(format!(
            "multiple open event segments for {tenant}:{entity_type}:{entity_id}"
        )));
    }
    drop(segment_rows);

    let mut max_rows = tx
        .query(
            "SELECT COALESCE(MAX(segment_index), -1)
             FROM (
               SELECT segment_index FROM events
               WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
               UNION ALL
               SELECT segment_index FROM event_segments
               WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
             )",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
    let last_segment_index = match max_rows.next().await.map_err(storage_error)? {
        Some(row) => row.get::<i64>(0).map_err(storage_error)?,
        None => -1,
    };
    drop(max_rows);
    if let Some((segment_index, start_sequence)) = open_segment {
        if segment_index != last_segment_index {
            return Err(PersistenceError::Storage(format!(
                "open event segment {segment_index} is not the latest segment {last_segment_index} for {tenant}:{entity_type}:{entity_id}"
            )));
        }
        let next_sequence = sqlite_sequence(
            current_sequence.checked_add(1).ok_or_else(|| {
                PersistenceError::Storage(
                    "event sequence exhausted while validating segment".to_string(),
                )
            })?,
            "segment validation",
        )?;
        if start_sequence <= 0 || start_sequence > next_sequence {
            return Err(PersistenceError::Storage(format!(
                "open event segment {segment_index} has invalid start sequence {start_sequence} for journal sequence {current_sequence}"
            )));
        }
        return Ok(segment_index);
    }
    let segment_index = last_segment_index
        .checked_add(1)
        .ok_or_else(|| PersistenceError::Storage("event segment index exhausted".to_string()))?;
    let start_sequence = current_sequence.checked_add(1).ok_or_else(|| {
        PersistenceError::Storage("event sequence exhausted while opening segment".to_string())
    })?;
    let start_sequence = sqlite_sequence(start_sequence, "segment creation")?;
    let inserted = tx
        .execute(
            "INSERT INTO event_segments
         (tenant, entity_type, entity_id, segment_index, start_sequence_nr)
         VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                tenant,
                entity_type,
                entity_id,
                segment_index,
                start_sequence
            ],
        )
        .await
        .map_err(storage_error)?;
    if inserted != 1 {
        return Err(PersistenceError::Storage(format!(
            "segment creation affected {inserted} rows, expected one"
        )));
    }
    Ok(segment_index)
}

async fn update_segment_after_append(
    tx: &libsql::Transaction,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    segment_index: i64,
    new_sequence: u64,
) -> Result<(), PersistenceError> {
    let new_sequence = sqlite_sequence(new_sequence, "segment update")?;
    let updated = tx
        .execute(
            "UPDATE event_segments
             SET end_sequence_nr = ?5, event_count = MAX(?5 - start_sequence_nr + 1, 0)
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND segment_index = ?4",
            params![tenant, entity_type, entity_id, segment_index, new_sequence],
        )
        .await
        .map_err(storage_error)?;
    if updated != 1 {
        return Err(PersistenceError::Storage(format!(
            "segment update affected {updated} rows, expected one"
        )));
    }
    Ok(())
}

async fn read_events_with_limit(
    store: &TursoEventStore,
    persistence_id: &str,
    from_sequence: u64,
    limit: i64,
) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
    debug_assert!(limit >= 0, "event read limit must be non-negative");
    let (tenant, entity_type, entity_id) =
        parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
    let conn = store.configured_connection().await?;
    let from_sequence = sqlite_sequence(from_sequence, "event read")?;

    let mut rows = conn
        .query(
            "SELECT sequence_nr, event_type, payload, metadata
             FROM events
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr > ?4
             ORDER BY sequence_nr ASC
             LIMIT ?5",
            params![tenant, entity_type, entity_id, from_sequence, limit],
        )
        .await
        .map_err(storage_error)?;

    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(storage_error)? {
        let seq = u64::try_from(row.get::<i64>(0).map_err(storage_error)?).map_err(|_| {
            PersistenceError::Serialization(format!(
                "journal {persistence_id} has a negative sequence"
            ))
        })?;
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
        // Each attempt is a complete append unit. Journal rows and segment
        // metadata commit in one transaction for every append size.
        // Event-store's UNIQUE (entity_type, entity_id, sequence_nr) makes
        // retries safe — if a prior attempt partially committed before erroring,
        // the retry's pre-check detects it as ConcurrencyViolation
        // (non-transient, propagates to caller via normal event-store contract).
        let attempt_timeout = append_attempt_timeout();
        let total_attempts = append_max_attempts();
        let mut last_err: Option<PersistenceError> = None;
        for attempt in 0..total_attempts {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(retry_delay_ms(attempt - 1))).await;
            }
            let _write_permit = self
                .acquire_write_permit("turso.append", WritePriority::High)
                .await?;
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

    async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        // ADR-0153: a single keyed probe of entity_key_index — present/absent in
        // O(log n), no candidate scan (the negative-existence access path). Bounded
        // regardless of how many entities the tenant/type holds, so it cannot trip
        // the scan budget that produces the 413.
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT entity_id FROM entity_key_index \
                 WHERE tenant = ?1 AND entity_type = ?2 AND key_name = ?3 AND key_hash = ?4",
                params![tenant, entity_type, key_name, key_hash],
            )
            .await
            .map_err(storage_error)?;
        match rows.next().await.map_err(storage_error)? {
            Some(row) => Ok(Some(row.get::<String>(0).map_err(storage_error)?)),
            None => Ok(None),
        }
    }

    // NOTE (ADR-0153): Turso intentionally does NOT implement `backfill_entity_keys`,
    // `mark_key_index_backfilled`, or `key_index_backfilled_types` — it keeps the
    // no-op/empty trait defaults. Turso never co-commits key rows (it does not override
    // `append_with_keys`), so its `entity_key_index` is never maintained on write. A
    // store that does not maintain the index live must NEVER become authoritative for
    // absence: backfilling or watermarking it would let a keyed miss wrongly read a
    // present entity as absent (or serve a stale keyed hit). Postgres (the current
    // query-plane backend) co-commits and is authoritative; the sim store does too for
    // DST. Giving Turso the keyed oracle requires first implementing live co-commit
    // (completing ADR-0153 phase 2 for Turso) — tracked separately.

    // ADR-0155: Turso maintains `entity_vector_index` **write-behind** — the event is
    // appended first (with retries), then the derived vector rows follow in a separate,
    // also-retried write. This is safe for vectors (unlike keys) because a vector row
    // carries no uniqueness constraint and a lagging index write only makes a ranking
    // temporarily incomplete; it can never corrupt a keyed absence. So Turso implements
    // the full vector surface below.
    async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        _key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconcile_vectors: bool,
    ) -> Result<u64, PersistenceError> {
        // The journal append is the durable event (keys are not maintained on Turso,
        // per the note above).
        let new_seq = self
            .append(persistence_id, expected_sequence, events)
            .await?;
        // Write-behind vector maintenance: reconcile the entity's rows (delete stale,
        // insert current — an empty `vector_rows` purges a deleted/cleared entity),
        // RETRIED like the event append rather than a warn-once one-shot, so a
        // transient failure does not silently drop the write. On final exhaustion the
        // error is logged loudly; the partition then lags until the next backfill
        // reconcile runs. Only runs when the type declares vector paths.
        if reconcile_vectors
            && let Ok((tenant, entity_type, entity_id)) = parse_persistence_id_parts(persistence_id)
        {
            let total_attempts = append_max_attempts();
            let mut last_err: Option<PersistenceError> = None;
            for attempt in 0..total_attempts {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_millis(retry_delay_ms(attempt - 1))).await;
                }
                match self
                    .backfill_entity_vectors(tenant, entity_type, entity_id, vector_rows)
                    .await
                {
                    Ok(()) => {
                        last_err = None;
                        break;
                    }
                    Err(err) => {
                        let transient = matches!(&err, PersistenceError::Storage(msg) if is_transient_write_error(msg));
                        last_err = Some(err);
                        if !transient {
                            break;
                        }
                    }
                }
            }
            if let Some(error) = last_err {
                error!(
                    persistence_id,
                    error = %error,
                    "turso vector-index write-behind failed after retries; partition lags until the next backfill reconcile"
                );
            }
        }
        Ok(new_seq)
    }

    async fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        vector_rows: &[EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        // Reconcile: DELETE all of the entity's rows, then insert the current ones.
        // Empty `vector_rows` purges the entity (deleted / un-embedded). Always runs
        // the delete so a purge is honored.
        let _write_permit = self
            .acquire_write_permit("turso.backfill_entity_vectors", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        tx.execute(
            "DELETE FROM entity_vector_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
        for row in vector_rows {
            tx.execute(
                "INSERT INTO entity_vector_index \
                 (tenant, entity_type, decl_name, model_tag, entity_id, vector, sequence_nr) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                params![
                    tenant,
                    entity_type,
                    row.decl_name.as_str(),
                    row.model_tag.as_str(),
                    entity_id,
                    Value::Blob(pack_f32_le(&row.vector)),
                ],
            )
            .await
            .map_err(storage_error)?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    async fn vector_candidates(
        &self,
        tenant: &str,
        entity_type: &str,
        decl_name: &str,
        model_tag: &str,
        limit: usize,
    ) -> Result<Vec<EntityVectorCandidate>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT entity_id, vector FROM entity_vector_index \
                 WHERE tenant = ?1 AND entity_type = ?2 AND decl_name = ?3 AND model_tag = ?4 \
                 ORDER BY entity_id LIMIT ?5",
                params![tenant, entity_type, decl_name, model_tag, limit as i64],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_id: String = row.get(0).map_err(storage_error)?;
            let bytes: Vec<u8> = row.get(1).map_err(storage_error)?;
            if let Some(vector) = unpack_f32_le(&bytes) {
                out.push(EntityVectorCandidate { entity_id, vector });
            }
        }
        Ok(out)
    }

    async fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        let _write_permit = self
            .acquire_write_permit("turso.mark_vector_index_backfilled", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let completed_at = temper_runtime::scheduler::sim_now().to_rfc3339();
        conn.execute(
            "INSERT INTO vector_index_backfill_watermark (tenant, entity_type, vector_set, completed_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(tenant, entity_type) \
             DO UPDATE SET vector_set = excluded.vector_set, completed_at = excluded.completed_at",
            params![tenant, entity_type, vector_set, completed_at.as_str()],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    async fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT entity_type, vector_set FROM vector_index_backfill_watermark \
                 WHERE tenant = ?1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_type: String = row.get(0).map_err(storage_error)?;
            let vector_set: String = row.get(1).map_err(storage_error)?;
            out.push((entity_type, vector_set));
        }
        Ok(out)
    }

    async fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT DISTINCT entity_id FROM entity_vector_index \
                 WHERE tenant = ?1 AND entity_type = ?2",
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(out)
    }

    #[instrument(skip_all, fields(otel.name = "turso.append_batch"))]
    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        EventStore::append_batch_guarded(self, appends, &[]).await
    }

    #[instrument(skip_all, fields(otel.name = "turso.append_batch_guarded"))]
    async fn append_batch_guarded(
        &self,
        appends: &[PersistenceAppend],
        guards: &[PersistenceSequenceGuard],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        validate_guarded_persistence_append_batch(appends, guards)?;
        if appends.is_empty() {
            return Ok(Vec::new());
        }
        if appends.iter().all(|append| append.events.is_empty()) {
            return Ok(appends
                .iter()
                .map(|append| PersistenceAppendResult {
                    persistence_id: append.persistence_id.clone(),
                    sequence_nr: append.expected_sequence,
                })
                .collect());
        }
        if guards.is_empty()
            && let [append] = appends
        {
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
                tokio::time::timeout(attempt_timeout, self.append_batch_inner(appends, guards))
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
        read_events_with_limit(self, persistence_id, from_sequence, i64::MAX).await
    }

    #[instrument(skip_all, fields(persistence_id, limit, otel.name = "turso.read_events_bounded"))]
    async fn read_events_bounded(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let limit = i64::try_from(limit).map_err(|_| {
            PersistenceError::Storage("event read limit exceeds libSQL range".to_string())
        })?;
        read_events_with_limit(self, persistence_id, from_sequence, limit).await
    }

    async fn read_latest_events(
        &self,
        persistence_ids: &[String],
    ) -> Result<Vec<Option<PersistenceEnvelope>>, PersistenceError> {
        super::latest_events::read_latest_events(self, persistence_ids).await
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
        let stored_sequence = sqlite_sequence(sequence_nr, "snapshot save")?;
        let _write_permit = self
            .acquire_write_permit("turso.save_snapshot", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        tx.execute(
            "INSERT INTO snapshots (tenant, entity_type, entity_id, sequence_nr, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (tenant, entity_type, entity_id)
             DO UPDATE SET
                sequence_nr = excluded.sequence_nr,
                snapshot = excluded.snapshot,
                created_at = datetime('now')
             WHERE excluded.sequence_nr >= snapshots.sequence_nr",
            params![
                tenant,
                entity_type,
                entity_id,
                stored_sequence,
                snapshot.to_vec()
            ],
        )
        .await
        .map_err(storage_error)?;

        tx.execute(
            "INSERT INTO snapshot_history (tenant, entity_type, entity_id, sequence_nr, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (tenant, entity_type, entity_id, sequence_nr)
             DO UPDATE SET snapshot = excluded.snapshot, created_at = datetime('now')",
            params![
                tenant,
                entity_type,
                entity_id,
                stored_sequence,
                snapshot.to_vec()
            ],
        )
        .await
        .map_err(storage_error)?;

        let mut tail_rows = tx
            .query(
                "SELECT COALESCE(MAX(sequence_nr), 0)
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
        let journal_tail = match tail_rows.next().await.map_err(storage_error)? {
            Some(row) => row.get::<i64>(0).map_err(storage_error)?,
            None => 0,
        };
        drop(tail_rows);
        tracing::debug!(
            persistence_id,
            stored_sequence,
            journal_tail,
            rotate_segment = journal_tail == stored_sequence && stored_sequence > 0,
            "evaluated snapshot segment rotation"
        );

        // Snapshot writes may be delayed behind later appends, and the raw
        // EventStore contract also permits snapshot-only test/utility streams.
        // Rotate event segments only when this snapshot is exactly the journal
        // tail; otherwise segment metadata would claim boundaries that the
        // journal does not have.
        if journal_tail == stored_sequence && stored_sequence > 0 {
            let mut segment_rows = tx
                .query(
                    "SELECT segment_index, MIN(sequence_nr), MAX(sequence_nr), COUNT(*)
                     FROM events
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                       AND segment_index = (
                         SELECT segment_index FROM events
                         WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                           AND sequence_nr = ?4
                       )
                     GROUP BY segment_index",
                    params![tenant, entity_type, entity_id, stored_sequence],
                )
                .await
                .map_err(storage_error)?;
            let Some(segment_row) = segment_rows.next().await.map_err(storage_error)? else {
                return Err(PersistenceError::Storage(format!(
                    "journal tail {stored_sequence} has no event segment"
                )));
            };
            let current_segment = segment_row.get::<i64>(0).map_err(storage_error)?;
            let segment_start = segment_row.get::<i64>(1).map_err(storage_error)?;
            let segment_end = segment_row.get::<i64>(2).map_err(storage_error)?;
            let event_count = segment_row.get::<i64>(3).map_err(storage_error)?;
            drop(segment_rows);

            let sealed = tx
                .execute(
                    "UPDATE event_segments
                     SET start_sequence_nr = ?5,
                         end_sequence_nr = ?6,
                         snapshot_sequence = ?7,
                         event_count = ?8,
                         sealed_at = datetime('now')
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                       AND segment_index = ?4",
                    params![
                        tenant,
                        entity_type,
                        entity_id,
                        current_segment,
                        segment_start,
                        segment_end,
                        stored_sequence,
                        event_count
                    ],
                )
                .await
                .map_err(storage_error)?;
            if sealed == 0 {
                tx.execute(
                    "INSERT INTO event_segments
                     (tenant, entity_type, entity_id, segment_index, start_sequence_nr,
                      end_sequence_nr, snapshot_sequence, event_count, sealed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'))",
                    params![
                        tenant,
                        entity_type,
                        entity_id,
                        current_segment,
                        segment_start,
                        segment_end,
                        stored_sequence,
                        event_count
                    ],
                )
                .await
                .map_err(storage_error)?;
            }
            let _ =
                open_segment_for_append(&tx, tenant, entity_type, entity_id, sequence_nr).await?;
        }

        tx.commit().await.map_err(storage_error)?;

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

        let sequence_nr =
            u64::try_from(row.get::<i64>(0).map_err(storage_error)?).map_err(|_| {
                PersistenceError::Serialization(format!(
                    "snapshot {persistence_id} has a negative sequence"
                ))
            })?;
        let snapshot = row.get::<Vec<u8>>(1).map_err(storage_error)?;
        Ok(Some((sequence_nr, snapshot)))
    }

    #[instrument(skip_all, fields(tenant, otel.name = "turso.list_entity_ids"))]
    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        // ARN-192: discovery is raw on every backend. Live-entity callers classify
        // these candidates through the bounded latest-event primitive and the
        // canonical `is_deletion_tombstone` predicate.
        let mut rows = conn
            .query(
                "SELECT DISTINCT e.entity_type, e.entity_id
                 FROM events e
                 WHERE e.tenant = ?1",
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
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT DISTINCT entity_id
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2
                 ORDER BY entity_id",
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(out)
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
                 UNION SELECT tenant FROM event_segments \
                 UNION SELECT tenant FROM snapshot_history \
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
            Some(row) => {
                u64::try_from(row.get::<i64>(0).map_err(storage_error)?).map_err(|_| {
                    PersistenceError::Serialization(format!(
                        "journal {persistence_id} has a negative sequence"
                    ))
                })?
            }
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

        let segment_index =
            open_segment_for_append(&tx, tenant, entity_type, entity_id, current_seq).await?;

        let mut new_seq = expected_sequence;
        for event in events {
            new_seq = new_seq.checked_add(1).ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "event sequence exhausted while appending {persistence_id}"
                ))
            })?;
            let stored_sequence = sqlite_sequence(new_seq, "event append")?;
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
                     (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        tenant,
                        entity_type,
                        entity_id,
                        stored_sequence,
                        segment_index,
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

        update_segment_after_append(&tx, tenant, entity_type, entity_id, segment_index, new_seq)
            .await?;

        tx.commit().await.map_err(storage_error)?;
        Ok(new_seq)
    }

    async fn append_batch_inner(
        &self,
        appends: &[PersistenceAppend],
        guards: &[PersistenceSequenceGuard],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        for guard in guards {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&guard.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            let mut rows = tx
                .query(
                    "SELECT COALESCE(MAX(sequence_nr), 0)
                     FROM events
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            let actual = match rows.next().await.map_err(storage_error)? {
                Some(row) => {
                    u64::try_from(row.get::<i64>(0).map_err(storage_error)?).map_err(|_| {
                        PersistenceError::Serialization(format!(
                            "journal {} has a negative sequence",
                            guard.persistence_id
                        ))
                    })?
                }
                None => 0,
            };
            drop(rows);
            if actual != guard.expected_sequence {
                let _ = tx.rollback().await;
                return Err(PersistenceError::PreconditionFailed {
                    persistence_id: guard.persistence_id.clone(),
                    expected: guard.expected_sequence,
                    actual,
                });
            }
        }

        let mut parsed = Vec::with_capacity(appends.len());
        for append in appends {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;

            if append.events.is_empty() {
                parsed.push((
                    tenant.to_string(),
                    entity_type.to_string(),
                    entity_id.to_string(),
                    None,
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
                Some(row) => {
                    u64::try_from(row.get::<i64>(0).map_err(storage_error)?).map_err(|_| {
                        PersistenceError::Serialization(format!(
                            "journal {} has a negative sequence",
                            append.persistence_id
                        ))
                    })?
                }
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
            let segment_index =
                open_segment_for_append(&tx, tenant, entity_type, entity_id, current_seq).await?;
            parsed.push((
                tenant.to_string(),
                entity_type.to_string(),
                entity_id.to_string(),
                Some(segment_index),
            ));
        }

        let mut results = Vec::with_capacity(appends.len());
        let mut event_rows = Vec::new();
        for (append, (tenant, entity_type, entity_id, segment_index)) in
            appends.iter().zip(parsed.iter())
        {
            let mut new_seq = append.expected_sequence;
            for event in &append.events {
                new_seq = new_seq.checked_add(1).ok_or_else(|| {
                    PersistenceError::Storage(format!(
                        "event sequence exhausted while appending {}",
                        append.persistence_id
                    ))
                })?;
                let stored_sequence = sqlite_sequence(new_seq, "batch event append")?;
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
                    stored_sequence_nr: stored_sequence,
                    segment_index: segment_index.expect("non-empty append has an event segment"),
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
                 (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata) \
                 VALUES ",
            );
            let mut insert_values = Vec::with_capacity(chunk.len() * 8);
            for (index, row) in chunk.iter().enumerate() {
                if index > 0 {
                    insert_sql.push_str(", ");
                }
                insert_sql.push_str("(?, ?, ?, ?, ?, ?, ?, ?)");
                insert_values.push(Value::from(row.tenant.clone()));
                insert_values.push(Value::from(row.entity_type.clone()));
                insert_values.push(Value::from(row.entity_id.clone()));
                insert_values.push(Value::from(row.stored_sequence_nr));
                insert_values.push(Value::from(row.segment_index));
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

        for (result, (_, (tenant, entity_type, entity_id, segment_index))) in
            results.iter().zip(appends.iter().zip(parsed.iter()))
        {
            if let Some(segment_index) = segment_index {
                update_segment_after_append(
                    &tx,
                    tenant,
                    entity_type,
                    entity_id,
                    *segment_index,
                    result.sequence_nr,
                )
                .await?;
            }
        }

        tx.commit().await.map_err(storage_error)?;
        Ok(results)
    }
}
