//! [`EventStore`] trait implementation for Turso/libSQL.

use libsql::{TransactionBehavior, Value, params, params_from_iter};
use std::time::Duration;
use temper_runtime::persistence::schema_deployment::{
    SchemaExecutionPin, SchemaScope, scoped_journal_pin_prefix, scoped_journal_pin_suffix,
    split_scoped_journal_entity_id,
};
use temper_runtime::persistence::{
    EntityVectorCandidate, EntityVectorRow, EventMetadata, EventStore, PersistenceAppend,
    PersistenceAppendResult, PersistenceEnvelope, PersistenceError, pack_f32_le, storage_error,
    unpack_f32_le,
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

async fn assert_scoped_journal_write_fence(
    tx: &libsql::Transaction,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    events: &[PersistenceEnvelope],
) -> Result<(), PersistenceError> {
    let Some((_, pin)) = split_scoped_journal_entity_id(entity_id) else {
        return Ok(());
    };
    let mut migrated_rows = tx
        .query(
            "SELECT 1 FROM schema_migration_jobs
             WHERE tenant = ?1
               AND json_extract(job_json, '$.command.source_bundle_digest') = ?2
               AND json_extract(job_json, '$.command.scope.kind') = 'task'
               AND json_extract(job_json, '$.command.scope.id') = ?3
               AND json_extract(job_json, '$.status') IN ('cut_over', 'completed')
             LIMIT 1",
            params![tenant, pin.bundle_digest.as_str(), pin.scope.id.as_str()],
        )
        .await
        .map_err(storage_error)?;
    if migrated_rows.next().await.map_err(storage_error)?.is_some() {
        return Err(PersistenceError::Storage(
            "migrated scoped schema write fence".into(),
        ));
    }
    for event in events
        .iter()
        .filter(|event| event.metadata.kernel.is_none())
    {
        let mut descriptor_fence_rows = tx
            .query(
                "SELECT 1 FROM schema_active_pointers AS pointers,
                     json_each(json_extract(pointers.pointer_json, '$.stream_publication_bindings')) AS binding
                 WHERE pointers.tenant = ?1
                   AND json_extract(pointers.pointer_json, '$.scope.kind') = 'task'
                   AND json_extract(pointers.pointer_json, '$.scope.id') = ?2
                   AND json_extract(pointers.pointer_json, '$.stream_fenced_source_bundle_digest') = ?3
                   AND binding.key = ?4 AND binding.value = ?5
                 LIMIT 1",
                params![
                    tenant,
                    pin.scope.id.as_str(),
                    pin.bundle_digest.as_str(),
                    entity_type,
                    event.event_type.as_str()
                ],
            )
            .await
            .map_err(storage_error)?;
        if descriptor_fence_rows
            .next()
            .await
            .map_err(storage_error)?
            .is_some()
        {
            return Err(PersistenceError::Storage(
                "stream descriptor source publication fence".into(),
            ));
        }
    }
    let mut existing_rows = tx
        .query(
            "SELECT 1 FROM events
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
             LIMIT 1",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
    if existing_rows.next().await.map_err(storage_error)?.is_some() {
        return Ok(());
    }
    let mut rows = tx
        .query(
            "SELECT 1 FROM schema_active_pointers
             WHERE tenant = ?1 AND json_extract(pointer_json, '$.bundle_digest') = ?2
               AND json_extract(pointer_json, '$.scope.kind') = 'task'
               AND json_extract(pointer_json, '$.scope.id') = ?3
             UNION ALL
             SELECT 1 FROM schema_migration_jobs
             WHERE tenant = ?1
               AND json_extract(job_json, '$.command.target_bundle_digest') = ?2
               AND json_extract(job_json, '$.command.scope.kind') = 'task'
               AND json_extract(job_json, '$.command.scope.id') = ?3
               AND json_extract(job_json, '$.status') IN
                   ('submitted', 'migrating', 'validating', 'ready')
             LIMIT 1",
            params![tenant, pin.bundle_digest, pin.scope.id],
        )
        .await
        .map_err(storage_error)?;
    if rows.next().await.map_err(storage_error)?.is_none() {
        return Err(PersistenceError::Storage(
            "stale scoped schema write fence".into(),
        ));
    }
    Ok(())
}

async fn unscoped_publication_is_fenced(
    tx: &libsql::Transaction,
    tenant: &str,
    entity_type: &str,
    event_type: &str,
) -> Result<bool, PersistenceError> {
    let mut rows = tx
        .query(
            "SELECT 1 FROM schema_active_pointers AS pointers,
                 json_each(json_extract(pointers.pointer_json, '$.bindings')) AS binding
             WHERE pointers.tenant = ?1
               AND pointers.scope_kind = 'installed_application'
               AND binding.key = ?2
               AND json_extract(binding.value, '$.publication_action') = ?3
             LIMIT 1",
            params![tenant, entity_type, event_type],
        )
        .await
        .map_err(storage_error)?;
    Ok(rows.next().await.map_err(storage_error)?.is_some())
}

async fn assert_unscoped_stream_publication_fence(
    tx: &libsql::Transaction,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    events: &[PersistenceEnvelope],
) -> Result<(), PersistenceError> {
    if split_scoped_journal_entity_id(entity_id).is_some() {
        return Ok(());
    }
    for event in events
        .iter()
        .filter(|event| event.metadata.kernel.is_none())
    {
        if unscoped_publication_is_fenced(tx, tenant, entity_type, &event.event_type).await? {
            return Err(PersistenceError::Storage(
                "stream descriptor publication fence".into(),
            ));
        }
    }
    Ok(())
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
        if appends.is_empty() {
            return Ok(Vec::new());
        }
        // Turso has no key query plane; match `append_with_index_rows` by
        // accepting and intentionally ignoring declared key rows.
        if let [append] = appends {
            let sequence_nr = self
                .append_with_index_rows(
                    &append.persistence_id,
                    append.expected_sequence,
                    &append.events,
                    &append.key_rows,
                    &append.vector_rows,
                    append.reconcile_vectors,
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

    async fn read_events_limited(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT sequence_nr, event_type, payload, metadata
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr > ?4
                 ORDER BY sequence_nr ASC LIMIT ?5",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    from_sequence.min(i64::MAX as u64) as i64,
                    limit.min(i64::MAX as usize) as i64
                ],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let metadata_raw = row
                .get::<Option<String>>(3)
                .map_err(storage_error)?
                .ok_or_else(|| PersistenceError::Serialization("missing event metadata".into()))?;
            out.push(PersistenceEnvelope {
                sequence_nr: row.get::<i64>(0).map_err(storage_error)? as u64,
                event_type: row.get::<String>(1).map_err(storage_error)?,
                payload: serde_json::from_str(&row.get::<String>(2).map_err(storage_error)?)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
                metadata: serde_json::from_str(&metadata_raw)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
            });
        }
        Ok(out)
    }

    async fn read_latest_events(
        &self,
        persistence_id: &str,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT sequence_nr, event_type, payload, metadata FROM (
                   SELECT sequence_nr, event_type, payload, metadata
                   FROM events
                   WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                   ORDER BY sequence_nr DESC LIMIT ?4
                 ) ORDER BY sequence_nr ASC",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    limit.min(i64::MAX as usize) as i64
                ],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let metadata_raw = row
                .get::<Option<String>>(3)
                .map_err(storage_error)?
                .ok_or_else(|| PersistenceError::Serialization("missing event metadata".into()))?;
            out.push(PersistenceEnvelope {
                sequence_nr: row.get::<i64>(0).map_err(storage_error)? as u64,
                event_type: row.get::<String>(1).map_err(storage_error)?,
                payload: serde_json::from_str(&row.get::<String>(2).map_err(storage_error)?)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
                metadata: serde_json::from_str(&metadata_raw)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
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

        tx.execute(
            "INSERT INTO snapshot_history (tenant, entity_type, entity_id, sequence_nr, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (tenant, entity_type, entity_id, sequence_nr)
             DO UPDATE SET snapshot = excluded.snapshot, created_at = datetime('now')",
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

        let mut segment_rows = tx
            .query(
                "SELECT COALESCE(MAX(segment_index), 0)
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr <= ?4",
                params![tenant, entity_type, entity_id, sequence_nr as i64],
            )
            .await
            .map_err(storage_error)?;
        let current_segment = match segment_rows.next().await.map_err(storage_error)? {
            Some(row) => row.get::<i64>(0).map_err(storage_error)?,
            None => 0,
        };
        drop(segment_rows);

        tx.execute(
            "INSERT INTO event_segments
             (tenant, entity_type, entity_id, segment_index, start_sequence_nr, end_sequence_nr, snapshot_sequence, event_count, sealed_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5, ?5, datetime('now'))
             ON CONFLICT(tenant, entity_type, entity_id, segment_index) DO NOTHING",
            params![
                tenant,
                entity_type,
                entity_id,
                current_segment,
                sequence_nr as i64
            ],
        )
        .await
        .map_err(storage_error)?;

        tx.execute(
            "UPDATE event_segments
             SET end_sequence_nr = ?5,
                 snapshot_sequence = ?5,
                 sealed_at = datetime('now'),
                 event_count = MAX(?5 - start_sequence_nr + 1, 0)
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND segment_index = ?4",
            params![
                tenant,
                entity_type,
                entity_id,
                current_segment,
                sequence_nr as i64
            ],
        )
        .await
        .map_err(storage_error)?;

        tx.execute(
            "INSERT INTO event_segments
             (tenant, entity_type, entity_id, segment_index, start_sequence_nr)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(tenant, entity_type, entity_id, segment_index) DO NOTHING",
            params![
                tenant,
                entity_type,
                entity_id,
                current_segment + 1,
                sequence_nr as i64 + 1
            ],
        )
        .await
        .map_err(storage_error)?;

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

    async fn list_journal_ids_page(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.configured_connection().await?;
        let limit = limit.min(i64::MAX as usize) as i64;
        let mut rows = if let (Some(entity_type), Some((after_type, after_id))) =
            (entity_type, after)
            && after_type == entity_type
        {
            conn.query(
                "SELECT DISTINCT entity_type, entity_id FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id > ?3
                 ORDER BY entity_type, entity_id LIMIT ?4",
                params![tenant, entity_type, after_id, limit],
            )
            .await
            .map_err(storage_error)?
        } else if let Some(entity_type) = entity_type
            && after.is_none_or(|(after_type, _)| after_type < entity_type)
        {
            conn.query(
                "SELECT DISTINCT entity_type, entity_id FROM events
                 WHERE tenant = ?1 AND entity_type = ?2
                 ORDER BY entity_type, entity_id LIMIT ?3",
                params![tenant, entity_type, limit],
            )
            .await
            .map_err(storage_error)?
        } else if entity_type.is_some() {
            conn.query("SELECT entity_type, entity_id FROM events WHERE 0 = 1", ())
                .await
                .map_err(storage_error)?
        } else if let Some((after_type, after_id)) = after {
            conn.query(
                "SELECT DISTINCT entity_type, entity_id FROM events
                 WHERE tenant = ?1
                   AND (entity_type > ?2 OR (entity_type = ?2 AND entity_id > ?3))
                 ORDER BY entity_type, entity_id LIMIT ?4",
                params![tenant, after_type, after_id, limit],
            )
            .await
            .map_err(storage_error)?
        } else {
            conn.query(
                "SELECT DISTINCT entity_type, entity_id FROM events
                 WHERE tenant = ?1 ORDER BY entity_type, entity_id LIMIT ?2",
                params![tenant, limit],
            )
            .await
            .map_err(storage_error)?
        };
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push((
                row.get::<String>(0).map_err(storage_error)?,
                row.get::<String>(1).map_err(storage_error)?,
            ));
        }
        Ok(out)
    }

    async fn list_unscoped_entity_ids_page(
        &self,
        tenant: &str,
        entity_type: &str,
        after_entity_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.configured_connection().await?;
        let limit = limit.min(i64::MAX as usize) as i64;
        let scoped_pattern = format!("*:schema:task:*:sha256:{}", "?".repeat(64));
        let mut rows = conn
            .query(
                "SELECT DISTINCT entity_id FROM events
                 WHERE tenant = ?1 AND entity_type = ?2
                   AND (?3 IS NULL OR entity_id > ?3)
                   AND entity_id NOT GLOB ?4
                 ORDER BY entity_id LIMIT ?5",
                params![tenant, entity_type, after_entity_id, scoped_pattern, limit],
            )
            .await
            .map_err(storage_error)?;
        let mut output = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            output.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(output)
    }

    async fn unscoped_entity_type_write_version(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<u64, PersistenceError> {
        let conn = self.configured_connection().await?;
        let scoped_pattern = format!("*:schema:task:*:sha256:{}", "?".repeat(64));
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id NOT GLOB ?3",
                params![tenant, entity_type, scoped_pattern],
            )
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(0);
        };
        let count = row.get::<i64>(0).map_err(storage_error)?;
        u64::try_from(count)
            .map_err(|_| PersistenceError::Storage("invalid global write version".into()))
    }

    async fn activate_unscoped_stream_publication_fence(
        &self,
        tenant: &str,
        fence: &temper_runtime::persistence::schema_deployment::StreamPublicationFence,
    ) -> Result<(), PersistenceError> {
        let temper_runtime::persistence::schema_deployment::StreamPublicationFence::InstalledApplication {
            application_id,
            semantic_digest: _,
            bindings,
        } = fence
        else {
            return Err(PersistenceError::Storage(
                "task fence cannot activate global stream publications".into(),
            ));
        };
        let _permit = self
            .acquire_write_permit("stream_publication_fence", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let scoped_pattern = format!("*:schema:task:*:sha256:{}", "?".repeat(64));
        for (entity_type, binding) in bindings {
            let mut rows = tx
                .query(
                    "SELECT COUNT(*) FROM events
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id NOT GLOB ?3",
                    params![tenant, entity_type.as_str(), scoped_pattern.as_str()],
                )
                .await
                .map_err(storage_error)?;
            let actual = if let Some(row) = rows.next().await.map_err(storage_error)? {
                let count = row.get::<i64>(0).map_err(storage_error)?;
                u64::try_from(count)
                    .map_err(|_| PersistenceError::Storage("invalid global write version".into()))?
            } else {
                0
            };
            drop(rows);
            if actual != binding.expected_write_version {
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: binding.expected_write_version,
                    actual,
                });
            }
        }
        let pointer_json = serde_json::to_string(fence)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        tx.execute(
            "INSERT INTO schema_active_pointers (tenant, scope_kind, scope_id, pointer_json)
             VALUES (?1, 'installed_application', ?2, ?3)
             ON CONFLICT(tenant, scope_kind, scope_id)
             DO UPDATE SET pointer_json = excluded.pointer_json",
            params![tenant, application_id.as_str(), pointer_json],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    async fn deactivate_unscoped_stream_publication_fence(
        &self,
        tenant: &str,
        application_id: &str,
        semantic_digest: Option<&str>,
    ) -> Result<(), PersistenceError> {
        let _permit = self
            .acquire_write_permit("stream_publication_fence_remove", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let mut rows = tx
            .query(
                "SELECT pointer_json FROM schema_active_pointers
                 WHERE tenant = ?1 AND scope_kind = 'installed_application' AND scope_id = ?2",
                params![tenant, application_id],
            )
            .await
            .map_err(storage_error)?;
        let pointer = rows
            .next()
            .await
            .map_err(storage_error)?
            .map(|row| row.get::<String>(0))
            .transpose()
            .map_err(storage_error)?;
        drop(rows);
        if let Some(pointer) = pointer {
            let fence: temper_runtime::persistence::schema_deployment::StreamPublicationFence =
                serde_json::from_str(&pointer)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            let temper_runtime::persistence::schema_deployment::StreamPublicationFence::InstalledApplication {
                application_id: found_application,
                semantic_digest: found_digest,
                ..
            } = fence
            else {
                return Err(PersistenceError::Storage(
                    "installed application publication fence is invalid".into(),
                ));
            };
            if found_application == application_id
                && semantic_digest.is_none_or(|expected| found_digest == expected)
            {
                tx.execute(
                    "DELETE FROM schema_active_pointers
                     WHERE tenant = ?1 AND scope_kind = 'installed_application' AND scope_id = ?2",
                    params![tenant, application_id],
                )
                .await
                .map_err(storage_error)?;
            }
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    async fn get_unscoped_stream_publication_fence(
        &self,
        tenant: &str,
        application_id: &str,
    ) -> Result<
        Option<temper_runtime::persistence::schema_deployment::StreamPublicationFence>,
        PersistenceError,
    > {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT pointer_json FROM schema_active_pointers
                 WHERE tenant = ?1 AND scope_kind = 'installed_application' AND scope_id = ?2",
                params![tenant, application_id],
            )
            .await
            .map_err(storage_error)?;
        rows.next()
            .await
            .map_err(storage_error)?
            .map(|row| row.get::<String>(0))
            .transpose()
            .map_err(storage_error)?
            .map(|pointer| serde_json::from_str(&pointer))
            .transpose()
            .map_err(|error| PersistenceError::Serialization(error.to_string()))
    }

    async fn unscoped_stream_publication_fence_active(
        &self,
        tenant: &str,
        entity_type: &str,
        publication_action: &str,
        capability_digest: &str,
    ) -> Result<bool, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT EXISTS (
                     SELECT 1 FROM schema_active_pointers AS pointers,
                         json_each(json_extract(pointers.pointer_json, '$.bindings')) AS binding
                     WHERE pointers.tenant = ?1
                       AND pointers.scope_kind = 'installed_application'
                       AND binding.key = ?2
                       AND json_extract(binding.value, '$.publication_action') = ?3
                       AND json_extract(binding.value, '$.capability_digest') = ?4
                 )",
                params![tenant, entity_type, publication_action, capability_digest],
            )
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(false);
        };
        row.get::<i64>(0)
            .map(|active| active != 0)
            .map_err(storage_error)
    }

    async fn restore_unscoped_stream_publication_fence(
        &self,
        tenant: &str,
        expected_current_semantic_digest: &str,
        fence: &temper_runtime::persistence::schema_deployment::StreamPublicationFence,
    ) -> Result<(), PersistenceError> {
        let temper_runtime::persistence::schema_deployment::StreamPublicationFence::InstalledApplication {
            application_id,
            ..
        } = fence
        else {
            return Err(PersistenceError::Storage(
                "task fence cannot restore global stream publications".into(),
            ));
        };
        let _permit = self
            .acquire_write_permit("stream_publication_fence_restore", WritePriority::High)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let mut rows = tx
            .query(
                "SELECT pointer_json FROM schema_active_pointers
                 WHERE tenant = ?1 AND scope_kind = 'installed_application' AND scope_id = ?2",
                params![tenant, application_id.clone()],
            )
            .await
            .map_err(storage_error)?;
        let current = rows
            .next()
            .await
            .map_err(storage_error)?
            .map(|row| row.get::<String>(0))
            .transpose()
            .map_err(storage_error)?;
        drop(rows);
        let Some(current) = current else {
            return Err(PersistenceError::Storage(
                "installed application publication fence changed concurrently".into(),
            ));
        };
        let current: temper_runtime::persistence::schema_deployment::StreamPublicationFence =
            serde_json::from_str(&current)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        if !matches!(
            current,
            temper_runtime::persistence::schema_deployment::StreamPublicationFence::InstalledApplication {
                semantic_digest,
                ..
            } if semantic_digest == expected_current_semantic_digest
        ) {
            return Err(PersistenceError::Storage(
                "installed application publication fence changed concurrently".into(),
            ));
        }
        let pointer = serde_json::to_string(fence)
            .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
        tx.execute(
            "INSERT INTO schema_active_pointers (tenant, scope_kind, scope_id, pointer_json)
             VALUES (?1, 'installed_application', ?2, ?3)
             ON CONFLICT(tenant, scope_kind, scope_id)
             DO UPDATE SET pointer_json = excluded.pointer_json",
            params![tenant, application_id.clone(), pointer],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }

    async fn list_scoped_entity_ids_page(
        &self,
        tenant: &str,
        entity_type: &str,
        scope: &SchemaScope,
        bundle_digest: &str,
        after_entity_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.configured_connection().await?;
        let suffix = scoped_journal_pin_suffix(&SchemaExecutionPin {
            scope: scope.clone(),
            bundle_digest: bundle_digest.to_string(),
        });
        let pattern = format!("%{suffix}");
        let after = after_entity_id.unwrap_or("");
        let limit = limit.min(i64::MAX as usize) as i64;
        let mut rows = conn
            .query(
                "SELECT DISTINCT substr(entity_id, 1, length(entity_id) - length(?3)) AS scoped_id
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id LIKE ?4
                   AND substr(entity_id, 1, length(entity_id) - length(?3)) > ?5
                 ORDER BY scoped_id LIMIT ?6",
                params![tenant, entity_type, suffix, pattern, after, limit],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(out)
    }

    async fn scoped_entity_bundle_digests(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        scope: &SchemaScope,
        limit: usize,
    ) -> Result<Vec<String>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.configured_connection().await?;
        let prefix = scoped_journal_pin_prefix(entity_id, scope);
        let prefix_len = i64::try_from(prefix.chars().count())
            .map_err(|_| PersistenceError::Storage("scoped entity id is too long".to_string()))?;
        let requested_limit = limit.min(i64::MAX as usize) as i64;
        let canonical_digest_glob = format!("sha256:{}", "[0-9a-f]".repeat(64));
        let mut rows = conn
            .query(
                "SELECT DISTINCT substr(entity_id, ?4 + 1) AS bundle_digest
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2
                   AND substr(entity_id, 1, ?4) = ?3
                   AND length(entity_id) = ?4 + 71
                   AND substr(entity_id, ?4 + 1) GLOB ?6
                 ORDER BY bundle_digest LIMIT ?5",
                params![
                    tenant,
                    entity_type,
                    prefix,
                    prefix_len,
                    requested_limit,
                    canonical_digest_glob
                ],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
        }
        Ok(out)
    }

    async fn scoped_bundle_write_version(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        bundle_digest: &str,
    ) -> Result<u64, PersistenceError> {
        let conn = self.configured_connection().await?;
        let suffix = scoped_journal_pin_suffix(&SchemaExecutionPin {
            scope: scope.clone(),
            bundle_digest: bundle_digest.to_string(),
        });
        let pattern = format!("%{suffix}");
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM events WHERE tenant = ?1 AND entity_id LIKE ?2",
                params![tenant, pattern],
            )
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(0);
        };
        let count = row.get::<i64>(0).map_err(storage_error)?;
        u64::try_from(count)
            .map_err(|_| PersistenceError::Storage("invalid schema write version".into()))
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

        if let [event] = events
            && !persistence_id.contains(":schema:")
        {
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

        assert_scoped_journal_write_fence(&tx, tenant, entity_type, entity_id, events).await?;
        assert_unscoped_stream_publication_fence(&tx, tenant, entity_type, entity_id, events)
            .await?;

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

        let segment_index = {
            let mut segment_rows = tx
                .query(
                    "SELECT segment_index
                     FROM event_segments
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sealed_at IS NULL
                     ORDER BY segment_index DESC
                     LIMIT 1",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            if let Some(row) = segment_rows.next().await.map_err(storage_error)? {
                row.get::<i64>(0).map_err(storage_error)?
            } else {
                drop(segment_rows);
                let mut max_rows = tx
                    .query(
                        "SELECT COALESCE(MAX(segment_index), 0)
                         FROM events
                         WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                        params![tenant, entity_type, entity_id],
                    )
                    .await
                    .map_err(storage_error)?;
                let idx = match max_rows.next().await.map_err(storage_error)? {
                    Some(row) => row.get::<i64>(0).map_err(storage_error)?,
                    None => 0,
                };
                drop(max_rows);
                tx.execute(
                    "INSERT INTO event_segments
                     (tenant, entity_type, entity_id, segment_index, start_sequence_nr)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(tenant, entity_type, entity_id, segment_index) DO NOTHING",
                    params![
                        tenant,
                        entity_type,
                        entity_id,
                        idx,
                        ((current_seq + 1).max(1)) as i64
                    ],
                )
                .await
                .map_err(storage_error)?;
                idx
            }
        };

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
                     (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        tenant,
                        entity_type,
                        entity_id,
                        new_seq as i64,
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

        if new_seq > expected_sequence {
            tx.execute(
                "UPDATE event_segments
                 SET end_sequence_nr = ?5, event_count = MAX(?5 - start_sequence_nr + 1, 0)
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND segment_index = ?4",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    segment_index,
                    new_seq as i64
                ],
            )
            .await
            .map_err(storage_error)?;
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
            assert_scoped_journal_write_fence(&tx, tenant, entity_type, entity_id, &append.events)
                .await?;
            assert_unscoped_stream_publication_fence(
                &tx,
                tenant,
                entity_type,
                entity_id,
                &append.events,
            )
            .await?;

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

        for ((append, (tenant, entity_type, entity_id)), result) in appends
            .iter()
            .zip(parsed.iter())
            .zip(results.iter())
            .filter(|((append, _), _)| append.reconcile_vectors)
        {
            tx.execute(
                "DELETE FROM entity_vector_index \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant.clone(), entity_type.clone(), entity_id.clone()],
            )
            .await
            .map_err(storage_error)?;
            for row in &append.vector_rows {
                tx.execute(
                    "INSERT INTO entity_vector_index \
                     (tenant, entity_type, decl_name, model_tag, entity_id, vector, sequence_nr) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        tenant.clone(),
                        entity_type.clone(),
                        row.decl_name.as_str(),
                        row.model_tag.as_str(),
                        entity_id.clone(),
                        Value::Blob(pack_f32_le(&row.vector)),
                        result.sequence_nr as i64,
                    ],
                )
                .await
                .map_err(storage_error)?;
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
        let segment_index = {
            let mut rows = conn
                .query(
                    "SELECT segment_index
                     FROM event_segments
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sealed_at IS NULL
                     ORDER BY segment_index DESC
                     LIMIT 1",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            if let Some(row) = rows.next().await.map_err(storage_error)? {
                row.get::<i64>(0).map_err(storage_error)?
            } else {
                drop(rows);
                let mut max_rows = conn
                    .query(
                        "SELECT COALESCE(MAX(segment_index), 0)
                         FROM events
                         WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                        params![tenant, entity_type, entity_id],
                    )
                    .await
                    .map_err(storage_error)?;
                let idx = match max_rows.next().await.map_err(storage_error)? {
                    Some(row) => row.get::<i64>(0).map_err(storage_error)?,
                    None => 0,
                };
                drop(max_rows);
                conn.execute(
                    "INSERT INTO event_segments
                     (tenant, entity_type, entity_id, segment_index, start_sequence_nr)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(tenant, entity_type, entity_id, segment_index) DO NOTHING",
                    params![
                        tenant,
                        entity_type,
                        entity_id,
                        idx,
                        ((expected_sequence + 1).max(1)) as i64
                    ],
                )
                .await
                .map_err(storage_error)?;
                idx
            }
        };
        let insert_result = conn
            .execute(
                "INSERT INTO events
                 (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
                 WHERE (
                     SELECT COALESCE(MAX(sequence_nr), 0)
                     FROM events
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                 ) = ?9
                   AND (?10 = 1 OR NOT EXISTS (
                       SELECT 1 FROM schema_active_pointers AS pointers,
                           json_each(json_extract(pointers.pointer_json, '$.bindings')) AS binding
                       WHERE pointers.tenant = ?1
                         AND pointers.scope_kind = 'installed_application'
                         AND binding.key = ?2
                         AND json_extract(binding.value, '$.publication_action') = ?6
                   ))",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    new_seq as i64,
                    segment_index,
                    event.event_type.as_str(),
                    payload_json,
                    metadata_json,
                    expected_sequence as i64,
                    i64::from(event.metadata.kernel.is_some())
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
            conn.execute(
                "UPDATE event_segments
                 SET end_sequence_nr = ?5, event_count = MAX(?5 - start_sequence_nr + 1, 0)
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND segment_index = ?4",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    segment_index,
                    new_seq as i64
                ],
            )
            .await
            .map_err(storage_error)?;
            return Ok(new_seq);
        }

        if event.metadata.kernel.is_none() {
            let mut rows = conn
                .query(
                    "SELECT 1 FROM schema_active_pointers AS pointers,
                         json_each(json_extract(pointers.pointer_json, '$.bindings')) AS binding
                     WHERE pointers.tenant = ?1
                       AND pointers.scope_kind = 'installed_application'
                       AND binding.key = ?2
                       AND json_extract(binding.value, '$.publication_action') = ?3
                     LIMIT 1",
                    params![tenant, entity_type, event.event_type.as_str()],
                )
                .await
                .map_err(storage_error)?;
            if rows.next().await.map_err(storage_error)?.is_some() {
                return Err(PersistenceError::Storage(
                    "stream descriptor publication fence".into(),
                ));
            }
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
