//! [`EventStore`] trait implementation for Turso/libSQL.

use libsql::{TransactionBehavior, Value, params, params_from_iter};
use std::time::Duration;
use temper_runtime::persistence::{
    EntityVectorCandidate, EntityVectorRow, EventMetadata, EventStore, PersistenceAppend,
    PersistenceAppendResult, PersistenceEnvelope, PersistenceError, pack_f32_le, storage_error,
    unpack_f32_le,
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
const ABSENT_DECLARATION_FINGERPRINT: &str = "absent:v1";

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

async fn current_vector_generation(
    tx: &libsql::Transaction,
    tenant: &str,
    entity_type: &str,
) -> Result<u64, PersistenceError> {
    tx.execute(
        "INSERT INTO entity_vector_reconciliation_generation \
         (tenant, entity_type, generation, vector_set) VALUES (?1, ?2, 0, '') \
         ON CONFLICT(tenant, entity_type) DO NOTHING",
        params![tenant, entity_type],
    )
    .await
    .map_err(storage_error)?;
    let mut rows = tx
        .query(
            "SELECT generation FROM entity_vector_reconciliation_generation \
             WHERE tenant = ?1 AND entity_type = ?2",
            params![tenant, entity_type],
        )
        .await
        .map_err(storage_error)?;
    let generation = rows
        .next()
        .await
        .map_err(storage_error)?
        .ok_or_else(|| {
            PersistenceError::Storage(format!(
                "missing vector reconciliation generation for {tenant}:{entity_type}"
            ))
        })?
        .get::<i64>(0)
        .map_err(storage_error)?;
    Ok(generation as u64)
}

async fn validate_spec_declaration_fingerprint(
    tx: &libsql::Transaction,
    tenant: &str,
    entity_type: &str,
    reconcile_vectors: bool,
    spec_declaration_fingerprint: Option<&str>,
) -> Result<(), PersistenceError> {
    let Some(provided_fingerprint) = spec_declaration_fingerprint else {
        if reconcile_vectors {
            return Err(PersistenceError::Storage(format!(
                "vector reconciliation append requires a spec declaration fingerprint for {tenant}:{entity_type}"
            )));
        }
        return Ok(());
    };
    if provided_fingerprint.is_empty() {
        return Err(PersistenceError::Storage(format!(
            "live append requires a nonempty spec declaration fingerprint for {tenant}:{entity_type}"
        )));
    }

    // Compatibility constructors can supply verified in-memory specs over a
    // truly empty store. Establish first-writer authority atomically only when
    // neither a durable catalog row nor a tombstone/authority row exists. Once
    // either exists, normal catalog mutation is the sole authority.
    tx.execute(
        "INSERT INTO spec_declaration_authority \
         (tenant, entity_type, revision, ioa_source, declaration_fingerprint, present) \
         SELECT ?1, ?2, 1, '', ?3, 1 \
         WHERE NOT EXISTS ( \
             SELECT 1 FROM specs WHERE tenant = ?1 AND entity_type = ?2 \
         ) \
         ON CONFLICT(tenant, entity_type) DO NOTHING",
        params![tenant, entity_type, provided_fingerprint],
    )
    .await
    .map_err(storage_error)?;

    let mut rows = tx
        .query(
            "SELECT ioa_source, declaration_fingerprint, present FROM spec_declaration_authority \
             WHERE tenant = ?1 AND entity_type = ?2",
            params![tenant, entity_type],
        )
        .await
        .map_err(storage_error)?;
    let authority = rows.next().await.map_err(storage_error)?.ok_or_else(|| {
        PersistenceError::Storage(format!(
            "missing durable spec declaration authority for {tenant}:{entity_type}"
        ))
    })?;
    let ioa_source = authority.get::<String>(0).map_err(storage_error)?;
    let stored_fingerprint = authority.get::<String>(1).map_err(storage_error)?;
    let present = authority.get::<i64>(2).map_err(storage_error)? != 0;
    drop(rows);

    let authoritative_fingerprint = if present {
        if stored_fingerprint.is_empty() {
            crate::spec_content_hash(&ioa_source)
        } else {
            stored_fingerprint
        }
    } else {
        ABSENT_DECLARATION_FINGERPRINT.to_string()
    };
    if authoritative_fingerprint != provided_fingerprint {
        return Err(PersistenceError::Storage(format!(
            "stale vector declaration fingerprint for {tenant}:{entity_type}"
        )));
    }
    Ok(())
}

async fn reconcile_live_vector_rows(
    tx: &libsql::Transaction,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    new_sequence: u64,
    vector_rows: &[EntityVectorRow],
) -> Result<(), PersistenceError> {
    let generation = current_vector_generation(tx, tenant, entity_type).await?;
    let applied = tx
        .execute(
            "INSERT INTO entity_vector_index_version \
             (tenant, entity_type, entity_id, reconciliation_generation, sequence_nr) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(tenant, entity_type, entity_id) DO UPDATE SET \
                 reconciliation_generation = excluded.reconciliation_generation, \
                 sequence_nr = excluded.sequence_nr \
             WHERE entity_vector_index_version.reconciliation_generation < excluded.reconciliation_generation \
                OR (entity_vector_index_version.reconciliation_generation = excluded.reconciliation_generation \
                    AND entity_vector_index_version.sequence_nr <= excluded.sequence_nr)",
            params![
                tenant,
                entity_type,
                entity_id,
                generation as i64,
                new_sequence as i64
            ],
        )
        .await
        .map_err(storage_error)?;
    if applied == 0 {
        return Err(PersistenceError::Storage(format!(
            "vector-index fence for {tenant}:{entity_type}:{entity_id} is ahead of live journal sequence {new_sequence} in reconciliation generation {generation}"
        )));
    }
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
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                tenant,
                entity_type,
                row.decl_name.as_str(),
                row.model_tag.as_str(),
                entity_id,
                Value::Blob(pack_f32_le(&row.vector)),
                new_sequence as i64,
            ],
        )
        .await
        .map_err(storage_error)?;
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
        self.append_retried(persistence_id, expected_sequence, events, None, None)
            .await
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

    // ADR-0181: Turso co-commits the journal, retained vector fence, and current
    // vector rows in one immediate transaction. The single-event fast path remains
    // available only to appends that neither reconcile vectors nor carry a spec
    // declaration fingerprint requiring transactional validation.
    async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        _key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconcile_vectors: bool,
        spec_declaration_fingerprint: Option<&str>,
    ) -> Result<u64, PersistenceError> {
        if !reconcile_vectors && spec_declaration_fingerprint.is_none() {
            return self.append(persistence_id, expected_sequence, events).await;
        }
        let vector_rows = reconcile_vectors.then_some(vector_rows);
        self.append_retried(
            persistence_id,
            expected_sequence,
            events,
            vector_rows,
            spec_declaration_fingerprint,
        )
        .await
    }

    async fn begin_vector_index_reconciliation(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
        declaration_revision: u64,
        declaration_fingerprint: &str,
    ) -> Result<u64, PersistenceError> {
        if declaration_revision == 0 || declaration_fingerprint.is_empty() {
            return Err(PersistenceError::Storage(format!(
                "vector declaration revision must be nonzero and fingerprinted for {tenant}:{entity_type}"
            )));
        }
        let _write_permit = self
            .acquire_write_permit(
                "turso.begin_vector_index_reconciliation",
                WritePriority::Low,
            )
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        validate_spec_declaration_fingerprint(
            &tx,
            tenant,
            entity_type,
            true,
            Some(declaration_fingerprint),
        )
        .await?;

        // The authority row survives hard spec deletion. Spec triggers advance it
        // and fence existing vector work within the same immediate transaction.
        let mut authority_rows = tx
            .query(
                "SELECT revision, ioa_source, declaration_fingerprint, present \
                 FROM spec_declaration_authority \
                 WHERE tenant = ?1 AND entity_type = ?2",
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;
        let authority = authority_rows
            .next()
            .await
            .map_err(storage_error)?
            .ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "missing durable spec declaration authority for {tenant}:{entity_type}"
                ))
            })?;
        let authoritative_revision = authority.get::<i64>(0).map_err(storage_error)?;
        let ioa_source = authority.get::<String>(1).map_err(storage_error)?;
        let authority_fingerprint = authority.get::<String>(2).map_err(storage_error)?;
        let present = authority.get::<i64>(3).map_err(storage_error)? != 0;
        drop(authority_rows);
        let stored_fingerprint = if present {
            if authority_fingerprint.is_empty() {
                crate::spec_content_hash(&ioa_source)
            } else {
                authority_fingerprint
            }
        } else {
            ABSENT_DECLARATION_FINGERPRINT.to_string()
        };
        if stored_fingerprint != declaration_fingerprint {
            return Err(PersistenceError::Storage(format!(
                "stale vector declaration fingerprint for {tenant}:{entity_type}"
            )));
        }
        let authoritative_revision = u64::try_from(authoritative_revision).map_err(|_| {
            PersistenceError::Storage(format!(
                "invalid durable spec revision for {tenant}:{entity_type}"
            ))
        })?;
        let stored_revision = i64::try_from(authoritative_revision).map_err(|_| {
            PersistenceError::Storage(format!(
                "vector declaration revision exhausted for {tenant}:{entity_type}"
            ))
        })?;

        let inserted = tx
            .execute(
            "INSERT INTO entity_vector_reconciliation_generation \
             (tenant, entity_type, generation, declaration_revision, declaration_fingerprint, vector_set) \
             VALUES (?1, ?2, 1, ?3, ?4, ?5) \
             ON CONFLICT(tenant, entity_type) DO NOTHING",
            params![
                tenant,
                entity_type,
                stored_revision,
                declaration_fingerprint,
                vector_set
            ],
        )
        .await
        .map_err(storage_error)?;
        if inserted == 1 {
            tx.execute(
                "DELETE FROM vector_index_backfill_watermark \
                 WHERE tenant = ?1 AND entity_type = ?2",
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;
            tx.commit().await.map_err(storage_error)?;
            return Ok(1);
        }

        let mut current_rows = tx
            .query(
                "SELECT generation, declaration_revision, declaration_fingerprint, vector_set \
                 FROM entity_vector_reconciliation_generation \
                 WHERE tenant = ?1 AND entity_type = ?2",
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;
        let current = current_rows
            .next()
            .await
            .map_err(storage_error)?
            .ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "missing vector reconciliation generation for {tenant}:{entity_type}"
                ))
            })?;
        let generation = current.get::<i64>(0).map_err(storage_error)?;
        let current_revision = current.get::<i64>(1).map_err(storage_error)?;
        let current_fingerprint = current.get::<String>(2).map_err(storage_error)?;
        let current_set = current.get::<String>(3).map_err(storage_error)?;
        drop(current_rows);

        let current_revision = u64::try_from(current_revision).map_err(|_| {
            PersistenceError::Storage(format!(
                "invalid vector declaration revision for {tenant}:{entity_type}"
            ))
        })?;
        if authoritative_revision < current_revision {
            return Err(PersistenceError::Storage(format!(
                "vector reconciliation revision {current_revision} exceeds declaration authority {authoritative_revision} for {tenant}:{entity_type}"
            )));
        }
        if authoritative_revision == current_revision {
            if current_fingerprint == declaration_fingerprint && current_set == vector_set {
                tx.commit().await.map_err(storage_error)?;
                return u64::try_from(generation).map_err(|_| {
                    PersistenceError::Storage(format!(
                        "invalid vector reconciliation generation for {tenant}:{entity_type}"
                    ))
                });
            }
            if !current_fingerprint.is_empty() || !current_set.is_empty() {
                return Err(PersistenceError::Storage(format!(
                    "conflicting vector declaration at revision {authoritative_revision} for {tenant}:{entity_type}"
                )));
            }
        }

        let next_generation = if authoritative_revision == current_revision {
            generation
        } else {
            generation.checked_add(1).ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "vector reconciliation generation exhausted for {tenant}:{entity_type}"
                ))
            })?
        };
        tx.execute(
            "UPDATE entity_vector_reconciliation_generation \
             SET generation = ?3, declaration_revision = ?4, \
                 declaration_fingerprint = ?5, vector_set = ?6 \
             WHERE tenant = ?1 AND entity_type = ?2",
            params![
                tenant,
                entity_type,
                next_generation,
                stored_revision,
                declaration_fingerprint,
                vector_set
            ],
        )
        .await
        .map_err(storage_error)?;
        // Claiming a trigger-advanced or upgraded revision withdraws any legacy
        // completion claim. An exact retry returned above leaves it intact.
        tx.execute(
            "DELETE FROM vector_index_backfill_watermark \
             WHERE tenant = ?1 AND entity_type = ?2",
            params![tenant, entity_type],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        u64::try_from(next_generation).map_err(|_| {
            PersistenceError::Storage(format!(
                "invalid vector reconciliation generation for {tenant}:{entity_type}"
            ))
        })
    }

    async fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        reconciliation_generation: u64,
        observed_sequence: u64,
        vector_rows: &[EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        if reconciliation_generation == 0 {
            return Err(PersistenceError::Storage(
                "vector reconciliation generation zero is reserved for pre-reconciliation live writes"
                    .to_string(),
            ));
        }
        let _write_permit = self
            .acquire_write_permit("turso.backfill_entity_vectors", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let current_generation = current_vector_generation(&tx, tenant, entity_type).await?;
        if current_generation != reconciliation_generation {
            let _ = tx.rollback().await;
            return Err(PersistenceError::Storage(format!(
                "stale vector reconciliation generation {reconciliation_generation} for {tenant}:{entity_type}; current generation is {current_generation}"
            )));
        }
        let applied = tx
            .execute(
                "INSERT INTO entity_vector_index_version \
                 (tenant, entity_type, entity_id, reconciliation_generation, sequence_nr) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(tenant, entity_type, entity_id) DO UPDATE SET \
                     reconciliation_generation = excluded.reconciliation_generation, \
                     sequence_nr = excluded.sequence_nr \
                 WHERE entity_vector_index_version.reconciliation_generation < excluded.reconciliation_generation \
                    OR (entity_vector_index_version.reconciliation_generation = excluded.reconciliation_generation \
                        AND entity_vector_index_version.sequence_nr <= excluded.sequence_nr)",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    reconciliation_generation as i64,
                    observed_sequence as i64
                ],
            )
            .await
            .map_err(storage_error)?;
        if applied == 0 {
            let mut rows = tx
                .query(
                    "SELECT reconciliation_generation FROM entity_vector_index_version \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            let fence_generation = rows
                .next()
                .await
                .map_err(storage_error)?
                .map(|row| row.get::<i64>(0).map_err(storage_error))
                .transpose()?
                .unwrap_or(0) as u64;
            drop(rows);
            if fence_generation > reconciliation_generation {
                let _ = tx.rollback().await;
                return Err(PersistenceError::Storage(format!(
                    "vector-index fence generation {fence_generation} is ahead of current type generation {reconciliation_generation} for {tenant}:{entity_type}:{entity_id}"
                )));
            }
            tx.commit().await.map_err(storage_error)?;
            return Ok(());
        }
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
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    tenant,
                    entity_type,
                    row.decl_name.as_str(),
                    row.model_tag.as_str(),
                    entity_id,
                    Value::Blob(pack_f32_le(&row.vector)),
                    observed_sequence as i64,
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
        reconciliation_generation: u64,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        if reconciliation_generation == 0 {
            return Err(PersistenceError::Storage(
                "vector reconciliation generation zero cannot publish a watermark".to_string(),
            ));
        }
        let _write_permit = self
            .acquire_write_permit("turso.mark_vector_index_backfilled", WritePriority::Low)
            .await?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;
        let mut generations = tx
            .query(
                "SELECT generation, vector_set FROM entity_vector_reconciliation_generation \
                 WHERE tenant = ?1 AND entity_type = ?2",
                params![tenant, entity_type],
            )
            .await
            .map_err(storage_error)?;
        let current = generations
            .next()
            .await
            .map_err(storage_error)?
            .map(|row| {
                Ok::<_, PersistenceError>((
                    row.get::<i64>(0).map_err(storage_error)? as u64,
                    row.get::<String>(1).map_err(storage_error)?,
                ))
            })
            .transpose()?;
        drop(generations);
        if current.as_ref().map(|(generation, signature)| {
            *generation == reconciliation_generation && signature == vector_set
        }) != Some(true)
        {
            let current_generation = current.map(|(generation, _)| generation).unwrap_or(0);
            let _ = tx.rollback().await;
            return Err(PersistenceError::Storage(format!(
                "stale vector reconciliation generation {reconciliation_generation} for {tenant}:{entity_type}; current generation is {current_generation}"
            )));
        }
        let completed_at = temper_runtime::scheduler::sim_now().to_rfc3339();
        tx.execute(
            "INSERT INTO vector_index_backfill_watermark (tenant, entity_type, vector_set, completed_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(tenant, entity_type) \
             DO UPDATE SET vector_set = excluded.vector_set, completed_at = excluded.completed_at",
            params![tenant, entity_type, vector_set, completed_at.as_str()],
        )
        .await
        .map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
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

    async fn vector_reconciliation_entity_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT entity_type FROM entity_vector_reconciliation_generation WHERE tenant = ?1 \
                 UNION SELECT entity_type FROM entity_vector_index_version WHERE tenant = ?1 \
                 UNION SELECT entity_type FROM entity_vector_index WHERE tenant = ?1 \
                 ORDER BY entity_type",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(row.get::<String>(0).map_err(storage_error)?);
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

    async fn list_vector_repair_entity_ids(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT DISTINCT entity_id FROM events \
                 WHERE tenant = ?1 AND entity_type = ?2 \
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
                .append_with_index_rows(
                    &append.persistence_id,
                    append.expected_sequence,
                    &append.events,
                    &[],
                    &append.vector_rows,
                    append.reconcile_vectors,
                    append.spec_declaration_fingerprint.as_deref(),
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
}

impl TursoEventStore {
    /// Retry one complete journal append, optionally including vector-index
    /// reconciliation in the same transaction (ADR-0181).
    async fn append_retried(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        vector_rows: Option<&[EntityVectorRow]>,
        spec_declaration_fingerprint: Option<&str>,
    ) -> Result<u64, PersistenceError> {
        if events.is_empty() && vector_rows.is_none() && spec_declaration_fingerprint.is_none() {
            return Ok(expected_sequence);
        }

        let attempt_timeout = append_attempt_timeout();
        let total_attempts = append_max_attempts();
        let mut last_err: Option<PersistenceError> = None;
        let bypass_write_gate =
            events.len() == 1 && vector_rows.is_none() && spec_declaration_fingerprint.is_none();
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
                self.append_inner(
                    persistence_id,
                    expected_sequence,
                    events,
                    vector_rows,
                    spec_declaration_fingerprint,
                ),
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
                Ok(sequence_nr) => {
                    if attempt > 0 {
                        record_turso_write_retry("turso.append", attempt as u64, "succeeded");
                    }
                    return Ok(sequence_nr);
                }
                Err(err) => {
                    let transient = match &err {
                        PersistenceError::Storage(message) => is_transient_write_error(message),
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
        vector_rows: Option<&[EntityVectorRow]>,
        spec_declaration_fingerprint: Option<&str>,
    ) -> Result<u64, PersistenceError> {
        if events.is_empty() && vector_rows.is_none() && spec_declaration_fingerprint.is_none() {
            return Ok(expected_sequence);
        }

        if vector_rows.is_none()
            && spec_declaration_fingerprint.is_none()
            && let [event] = events
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

        validate_spec_declaration_fingerprint(
            &tx,
            tenant,
            entity_type,
            vector_rows.is_some(),
            spec_declaration_fingerprint,
        )
        .await?;
        if events.is_empty() && vector_rows.is_none() {
            tx.commit().await.map_err(storage_error)?;
            return Ok(expected_sequence);
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

        if let Some(vector_rows) = vector_rows {
            reconcile_live_vector_rows(&tx, tenant, entity_type, entity_id, new_seq, vector_rows)
                .await?;
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

            validate_spec_declaration_fingerprint(
                &tx,
                tenant,
                entity_type,
                append.reconcile_vectors,
                append.spec_declaration_fingerprint.as_deref(),
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

        for ((append, result), (tenant, entity_type, entity_id)) in
            appends.iter().zip(results.iter()).zip(parsed.iter())
        {
            if append.reconcile_vectors {
                reconcile_live_vector_rows(
                    &tx,
                    tenant,
                    entity_type,
                    entity_id,
                    result.sequence_nr,
                    &append.vector_rows,
                )
                .await?;
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
                 ) = ?9",
                params![
                    tenant,
                    entity_type,
                    entity_id,
                    new_seq as i64,
                    segment_index,
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
