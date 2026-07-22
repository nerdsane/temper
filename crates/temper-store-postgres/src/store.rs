//! PostgreSQL-backed implementation of the [`EventStore`] trait.
//!
//! The store uses a `sqlx::PgPool` for all database access and relies on the
//! `UNIQUE (entity_type, entity_id, sequence_nr)` constraint to enforce
//! optimistic concurrency on appends.

mod append;
mod append_batch;
mod key_index;
mod snapshot;

#[cfg(test)]
mod tests;

use std::time::Instant;

use sqlx::{Acquire, PgConnection, PgPool};
use temper_runtime::persistence::{
    EntityKeyLookup, EntityVectorCandidate, EntityVectorRow, EventMetadata, EventStore,
    IndexReconciliation, JournalBoundary, KeyContractActivation, PersistenceAppend,
    PersistenceAppendResult, PersistenceBatchIdempotency, PersistenceEnvelope, PersistenceError,
    SnapshotSourceFence, is_state_materialization_event_for, is_state_materialization_payload_for,
    pack_f32_le, unpack_f32_le,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use crate::metrics::{
    PostgresTransactionTimer, record_postgres_pool_acquire_duration,
    record_postgres_transaction_begin_duration, record_postgres_transaction_commit_duration,
};
use crate::segments;

pub(crate) use key_index::{
    DerivedWriteSource, KeyContractUse, event_stream_lock_key,
    invalidate_key_coverage_for_derived_write, invalidate_key_coverage_for_unreconciled_append,
    lock_event_stream, lock_key_contract, reconcile_key_contract_state,
};

const EVENT_APPEND_OPERATION: &str = "event_append";

fn snapshot_source_matches(
    current: Option<&(i64, Vec<u8>)>,
    expected: &SnapshotSourceFence,
) -> bool {
    match expected {
        SnapshotSourceFence::Unchecked => true,
        SnapshotSourceFence::Absent => current.is_none(),
        SnapshotSourceFence::Exact { sequence_nr, state } => current.is_some_and(|current| {
            u64::try_from(current.0).ok() == Some(*sequence_nr)
                && current.1.as_slice() == state.as_slice()
        }),
    }
}

fn append_retires_snapshot(
    expected_sequence: u64,
    events: &[PersistenceEnvelope],
    snapshot_source: &SnapshotSourceFence,
    entity_type: &str,
    entity_id: &str,
) -> bool {
    expected_sequence == 0
        && matches!(snapshot_source, SnapshotSourceFence::Exact { .. })
        && events
            .first()
            .is_some_and(|event| is_state_materialization_event_for(event, entity_type, entity_id))
}

async fn load_snapshot_for_update(
    conn: &mut PgConnection,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
) -> Result<Option<(i64, Vec<u8>)>, PersistenceError> {
    crate::dbm::postgres_query_as!(
        "SELECT sequence_nr, state FROM snapshots \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
         FOR UPDATE",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .fetch_optional(conn)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))
}

pub(crate) async fn mark_query_projection_dirty(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
) -> Result<(), PersistenceError> {
    crate::dbm::postgres_query!(
        "INSERT INTO query_projection_dirty (tenant, entity_type, entity_id, updated_at) \
         VALUES ($1, $2, $3, now()) \
         ON CONFLICT (tenant, entity_type, entity_id) \
         DO UPDATE SET updated_at = now()",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
    Ok(())
}

pub(crate) async fn clear_query_projection_dirty(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
) -> Result<(), PersistenceError> {
    crate::dbm::postgres_query!(
        "DELETE FROM query_projection_dirty \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
    Ok(())
}

/// A PostgreSQL-backed event store.
///
/// Persistence IDs follow `"tenant:entity_type:entity_id"` (with legacy
/// `"entity_type:entity_id"` mapped to tenant `"default"`). Components are
/// stored in separate columns for efficient filtering.
#[derive(Clone, Debug)]
pub struct PostgresEventStore {
    pool: PgPool,
}

impl PostgresEventStore {
    /// Create a new store backed by the given connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Return a reference to the inner pool (useful for migrations).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

// ---------------------------------------------------------------------------
// EventStore implementation
// ---------------------------------------------------------------------------

impl EventStore for PostgresEventStore {
    fn supports_authoritative_key_index(&self) -> bool {
        true
    }

    async fn batch_idempotency_committed(
        &self,
        claim: &PersistenceBatchIdempotency,
    ) -> Result<bool, PersistenceError> {
        let existing: Option<(String,)> = crate::dbm::postgres_query_as!(
            "SELECT intent_hash FROM persistence_batch_idempotency \
             WHERE persistence_id = $1 AND idempotency_key = $2",
        )
        .bind(&claim.persistence_id)
        .bind(&claim.idempotency_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        let Some((committed_hash,)) = existing else {
            return Ok(false);
        };
        if committed_hash != claim.intent_hash {
            return Err(PersistenceError::Storage(format!(
                "atomic batch idempotency key '{}' was reused with a different intent",
                claim.idempotency_key
            )));
        }
        Ok(true)
    }

    /// Append one or more events to the journal.
    ///
    /// Events are inserted with consecutive sequence numbers starting from
    /// `expected_sequence + 1`.  The UNIQUE index on
    /// `(entity_type, entity_id, sequence_nr)` enforces optimistic
    /// concurrency; a duplicate-key violation is surfaced as
    /// [`PersistenceError::ConcurrencyViolation`].
    ///
    /// Returns the new highest sequence number after the append.
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.append_with_index_rows(
            persistence_id,
            expected_sequence,
            events,
            &[],
            &[],
            IndexReconciliation::default(),
        )
        .await
    }

    async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconciliation: IndexReconciliation,
    ) -> Result<u64, PersistenceError> {
        self.append_with_index_rows_inner(
            persistence_id,
            expected_sequence,
            events,
            key_rows,
            vector_rows,
            reconciliation,
        )
        .await
    }

    async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        expected_sequence: u64,
        contract_fence: temper_runtime::persistence::KeyIndexBackfillFence<'_>,
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        key_index::backfill_entity_keys(
            &self.pool,
            tenant,
            entity_type,
            entity_id,
            expected_sequence,
            contract_fence,
            key_rows,
        )
        .await
    }

    async fn mark_key_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<(), PersistenceError> {
        key_index::mark_backfilled(&self.pool, tenant, entity_type, key_set).await
    }

    async fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        key_index::backfilled_types(&self.pool, tenant).await
    }

    async fn key_index_activated_contracts(
        &self,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        key_index::activated_contracts(&self.pool).await
    }

    async fn key_index_reconciliation_revision(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<u64, PersistenceError> {
        key_index::reconciliation_revision(&self.pool, tenant, entity_type).await
    }

    async fn begin_key_index_backfill(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<u64, PersistenceError> {
        key_index::begin_backfill(&self.pool, tenant, entity_type, key_set).await
    }

    async fn activate_key_index_contract(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
        purge_existing_rows: bool,
    ) -> Result<u64, PersistenceError> {
        key_index::activate_contract(
            &self.pool,
            tenant,
            entity_type,
            key_set,
            purge_existing_rows,
        )
        .await
    }

    async fn activate_key_index_contracts(
        &self,
        tenant: &str,
        activations: &[KeyContractActivation],
    ) -> Result<std::collections::BTreeMap<String, u64>, PersistenceError> {
        key_index::activate_contracts(&self.pool, tenant, activations).await
    }

    async fn mark_key_index_backfilled_if_revision(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
        expected_revision: u64,
    ) -> Result<bool, PersistenceError> {
        key_index::mark_backfilled_if_revision(
            &self.pool,
            tenant,
            entity_type,
            key_set,
            expected_revision,
        )
        .await
    }

    async fn keyed_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        key_index::keyed_entity_ids(&self.pool, tenant, entity_type).await
    }

    async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        Ok(self
            .lookup_by_key_with_sequence(tenant, entity_type, key_name, key_hash)
            .await?
            .map(|lookup| lookup.entity_id))
    }

    async fn lookup_by_key_with_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<EntityKeyLookup>, PersistenceError> {
        key_index::lookup(&self.pool, tenant, entity_type, key_name, key_hash).await
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
        // the delete (even for empty rows) so a purge is honored.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        crate::dbm::postgres_query!(
            "DELETE FROM entity_vector_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        for row in vector_rows {
            crate::dbm::postgres_query!(
                "INSERT INTO entity_vector_index \
                 (tenant, entity_type, decl_name, model_tag, entity_id, vector, sequence_nr) \
                 VALUES ($1, $2, $3, $4, $5, $6, 0)",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(&row.decl_name)
            .bind(&row.model_tag)
            .bind(entity_id)
            .bind(pack_f32_le(&row.vector))
            .execute(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
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
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        let rows: Vec<(String, Vec<u8>)> = crate::dbm::postgres_query_as!(
            "SELECT entity_id, vector FROM entity_vector_index \
             WHERE tenant = $1 AND entity_type = $2 AND decl_name = $3 AND model_tag = $4 \
             ORDER BY entity_id LIMIT $5",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(decl_name)
        .bind(model_tag)
        .bind(limit as i64)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(rows
            .into_iter()
            .filter_map(|(entity_id, bytes)| {
                unpack_f32_le(&bytes).map(|vector| EntityVectorCandidate { entity_id, vector })
            })
            .collect())
    }

    async fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        crate::dbm::postgres_query!(
            "INSERT INTO vector_index_backfill_watermark (tenant, entity_type, vector_set) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (tenant, entity_type) \
             DO UPDATE SET vector_set = EXCLUDED.vector_set, completed_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(vector_set)
        .execute(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(
            "SELECT entity_type, vector_set FROM vector_index_backfill_watermark WHERE tenant = $1",
        )
        .bind(tenant)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(rows.into_iter().collect())
    }

    async fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        let rows: Vec<(String,)> = crate::dbm::postgres_query_as!(
            "SELECT DISTINCT entity_id FROM entity_vector_index \
             WHERE tenant = $1 AND entity_type = $2",
        )
        .bind(tenant)
        .bind(entity_type)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(rows.into_iter().map(|(entity_id,)| entity_id).collect())
    }

    /// Atomically append to multiple entity journals in one PostgreSQL transaction.
    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        self.append_batch_inner(appends).await
    }

    /// Read events from the journal starting after `from_sequence`.
    ///
    /// Events are returned in ascending `sequence_nr` order.
    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;

        let rows: Vec<(i64, String, serde_json::Value, serde_json::Value)> =
            crate::dbm::postgres_query_as!(
                "SELECT sequence_nr, event_type, payload, metadata \
             FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr > $4 \
             ORDER BY sequence_nr ASC",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(from_sequence as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        rows.into_iter()
            .map(|(seq, event_type, payload, meta_json)| {
                let metadata: EventMetadata = serde_json::from_value(meta_json)
                    .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
                Ok(PersistenceEnvelope {
                    sequence_nr: seq as u64,
                    event_type,
                    payload,
                    metadata,
                })
            })
            .collect()
    }

    async fn read_events_page(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        through_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        assert!(limit > 0, "event page limit must be positive");
        assert!(
            through_sequence >= from_sequence,
            "event page boundary must not precede its cursor"
        );
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let from_sequence = i64::try_from(from_sequence).map_err(|_| {
            PersistenceError::Storage("event page cursor exceeds PostgreSQL bigint".to_string())
        })?;
        let through_sequence = i64::try_from(through_sequence).map_err(|_| {
            PersistenceError::Storage("event page boundary exceeds PostgreSQL bigint".to_string())
        })?;
        let limit = i64::try_from(limit).map_err(|_| {
            PersistenceError::Storage("event page limit exceeds PostgreSQL bigint".to_string())
        })?;

        let rows: Vec<(i64, String, serde_json::Value, serde_json::Value)> =
            crate::dbm::postgres_query_as!(
                "SELECT sequence_nr, event_type, payload, metadata \
                 FROM events \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                   AND sequence_nr > $4 AND sequence_nr <= $5 \
                 ORDER BY sequence_nr ASC \
                 LIMIT $6",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(from_sequence)
            .bind(through_sequence)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;

        rows.into_iter()
            .map(|(sequence_nr, event_type, payload, metadata)| {
                let metadata = serde_json::from_value(metadata)
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
                Ok(PersistenceEnvelope {
                    sequence_nr: sequence_nr as u64,
                    event_type,
                    payload,
                    metadata,
                })
            })
            .collect()
    }

    async fn journal_boundary(
        &self,
        persistence_id: &str,
    ) -> Result<JournalBoundary, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let row: (i64, Option<i64>) = crate::dbm::postgres_query_as!(
            "SELECT COALESCE(MAX(sequence_nr), 0), \
               MIN(sequence_nr) FILTER (WHERE \
                 CASE WHEN jsonb_typeof(payload) = 'object' AND payload ? 'to_status' \
                   THEN payload ->> 'to_status' = 'Deleted' \
                   ELSE event_type = 'Deleted' OR payload ->> 'action' = 'Deleted' \
                 END) \
             FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(JournalBoundary {
            latest_sequence: row.0 as u64,
            first_terminal_sequence: row.1.map(|sequence_nr| sequence_nr as u64),
        })
    }

    /// Save (upsert) a snapshot for the given entity.
    ///
    /// Uses `ON CONFLICT … DO UPDATE` so that only the latest snapshot is
    /// retained per entity.
    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        self.save_snapshot_inner(
            persistence_id,
            sequence_nr,
            snapshot,
            &SnapshotSourceFence::Unchecked,
            None,
        )
        .await
    }

    async fn save_snapshot_if_source(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
        source: &SnapshotSourceFence,
        key_contract: Option<&str>,
    ) -> Result<(), PersistenceError> {
        self.save_snapshot_inner(persistence_id, sequence_nr, snapshot, source, key_contract)
            .await
    }

    /// Load the latest snapshot for an entity.
    ///
    /// Returns `None` when no snapshot has been saved yet.
    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;

        let row: Option<(i64, Vec<u8>)> = crate::dbm::postgres_query_as!(
            "SELECT sequence_nr, state FROM snapshots \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(row.map(|(seq, state)| (seq as u64, state)))
    }

    /// List all distinct entities that have at least one persisted event
    /// in the given tenant.
    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(
            "SELECT DISTINCT entity_type, entity_id \
             FROM events \
             WHERE tenant = $1",
        )
        .bind(tenant)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(rows)
    }

    /// List distinct entity IDs for one entity type in the given tenant.
    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let rows: Vec<String> = crate::dbm::postgres_query_scalar!(
            "SELECT entity_id \
             FROM ( \
               SELECT c.entity_id \
               FROM entity_catalog c \
               WHERE c.tenant = $1 \
                 AND c.entity_type = $2 \
                 AND NOT EXISTS ( \
                   SELECT 1 \
                   FROM events d \
                   WHERE d.tenant = c.tenant \
                     AND d.entity_type = c.entity_type \
                     AND d.entity_id = c.entity_id \
                     AND (CASE WHEN jsonb_typeof(d.payload) = 'object' AND d.payload ? 'to_status' \
                       THEN d.payload ->> 'to_status' = 'Deleted' \
                       ELSE d.event_type = 'Deleted' OR d.payload ->> 'action' = 'Deleted' \
                     END) \
                 ) \
               UNION \
               SELECT f.entity_id \
               FROM entity_field_index f \
               WHERE f.tenant = $1 \
                 AND f.entity_type = $2 \
                 AND NOT EXISTS ( \
                   SELECT 1 \
                   FROM events d \
                   WHERE d.tenant = f.tenant \
                     AND d.entity_type = f.entity_type \
                     AND d.entity_id = f.entity_id \
                     AND (CASE WHEN jsonb_typeof(d.payload) = 'object' AND d.payload ? 'to_status' \
                       THEN d.payload ->> 'to_status' = 'Deleted' \
                       ELSE d.event_type = 'Deleted' OR d.payload ->> 'action' = 'Deleted' \
                     END) \
                 ) \
               UNION \
               SELECT s.entity_id \
               FROM snapshots s \
               WHERE s.tenant = $1 \
                 AND s.entity_type = $2 \
                 AND NOT EXISTS ( \
                   SELECT 1 \
                   FROM events d \
                   WHERE d.tenant = s.tenant \
                     AND d.entity_type = s.entity_type \
                     AND d.entity_id = s.entity_id \
                     AND (CASE WHEN jsonb_typeof(d.payload) = 'object' AND d.payload ? 'to_status' \
                       THEN d.payload ->> 'to_status' = 'Deleted' \
                       ELSE d.event_type = 'Deleted' OR d.payload ->> 'action' = 'Deleted' \
                     END) \
                 ) \
               UNION \
               SELECT DISTINCT e.entity_id \
               FROM events e \
               WHERE e.tenant = $1 \
                 AND e.entity_type = $2 \
                 AND NOT EXISTS ( \
                   SELECT 1 \
                   FROM events d \
                   WHERE d.tenant = e.tenant \
                     AND d.entity_type = e.entity_type \
                     AND d.entity_id = e.entity_id \
                     AND (CASE WHEN jsonb_typeof(d.payload) = 'object' AND d.payload ? 'to_status' \
                       THEN d.payload ->> 'to_status' = 'Deleted' \
                       ELSE d.event_type = 'Deleted' OR d.payload ->> 'action' = 'Deleted' \
                     END) \
                 ) \
             ) ids \
             ORDER BY entity_id",
        )
        .bind(tenant)
        .bind(entity_type)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(rows)
    }

    async fn list_entity_ids_for_key_reconciliation(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        // Repair authority is deliberately broader than live query enumeration:
        // deleted streams can retain pre-v3 rows, while interrupted/manual legacy
        // writes can leave key rows whose journal no longer exists. Migration-era
        // catalog, field-index, and snapshot rows may also be the only durable copy
        // of an entity. Every source must therefore participate in repair coverage.
        let rows: Vec<String> = crate::dbm::postgres_query_scalar!(
            "SELECT entity_id \
             FROM ( \
               SELECT DISTINCT e.entity_id \
               FROM events e \
               WHERE e.tenant = $1 AND e.entity_type = $2 \
               UNION \
               SELECT DISTINCT k.entity_id \
               FROM entity_key_index k \
               WHERE k.tenant = $1 AND k.entity_type = $2 \
               UNION \
               SELECT c.entity_id \
               FROM entity_catalog c \
               WHERE c.tenant = $1 AND c.entity_type = $2 \
               UNION \
               SELECT DISTINCT f.entity_id \
               FROM entity_field_index f \
               WHERE f.tenant = $1 AND f.entity_type = $2 \
               UNION \
               SELECT s.entity_id \
               FROM snapshots s \
               WHERE s.tenant = $1 AND s.entity_type = $2 \
             ) repair_ids \
             ORDER BY entity_id",
        )
        .bind(tenant)
        .bind(entity_type)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(rows)
    }

    async fn list_key_reconciliation_page(
        &self,
        tenant: &str,
        entity_type: &str,
        after_entity_id: Option<&str>,
        through_entity_id: &str,
        limit: usize,
    ) -> Result<Vec<temper_runtime::persistence::KeyReconciliationEntity>, PersistenceError> {
        assert!(limit > 0, "key reconciliation page limit must be positive");
        let limit = limit.min(i64::MAX as usize) as i64;
        let rows: Vec<(String, bool)> = crate::dbm::postgres_query_as!(
            "WITH repair_ids AS ( \
               SELECT DISTINCT e.entity_id FROM events e \
               WHERE e.tenant = $1 AND e.entity_type = $2 \
               UNION \
               SELECT DISTINCT k.entity_id FROM entity_key_index k \
               WHERE k.tenant = $1 AND k.entity_type = $2 \
               UNION \
               SELECT c.entity_id FROM entity_catalog c \
               WHERE c.tenant = $1 AND c.entity_type = $2 \
               UNION \
               SELECT DISTINCT f.entity_id FROM entity_field_index f \
               WHERE f.tenant = $1 AND f.entity_type = $2 \
               UNION \
               SELECT s.entity_id FROM snapshots s \
               WHERE s.tenant = $1 AND s.entity_type = $2 \
             ), source_ids AS ( \
               SELECT DISTINCT e.entity_id FROM events e \
               WHERE e.tenant = $1 AND e.entity_type = $2 \
               UNION \
               SELECT c.entity_id FROM entity_catalog c \
               WHERE c.tenant = $1 AND c.entity_type = $2 \
               UNION \
               SELECT DISTINCT f.entity_id FROM entity_field_index f \
               WHERE f.tenant = $1 AND f.entity_type = $2 \
               UNION \
               SELECT s.entity_id FROM snapshots s \
               WHERE s.tenant = $1 AND s.entity_type = $2 \
             ) \
             SELECT r.entity_id, \
                    EXISTS (SELECT 1 FROM source_ids s WHERE s.entity_id = r.entity_id) \
                    AND NOT EXISTS ( \
                      SELECT 1 FROM events d \
                      WHERE d.tenant = $1 AND d.entity_type = $2 \
                        AND d.entity_id = r.entity_id \
                        AND (CASE WHEN jsonb_typeof(d.payload) = 'object' AND d.payload ? 'to_status' \
                          THEN d.payload ->> 'to_status' = 'Deleted' \
                          ELSE d.event_type = 'Deleted' OR d.payload ->> 'action' = 'Deleted' \
                        END) \
                    ) AS is_live \
             FROM repair_ids r \
             WHERE ($3::text IS NULL OR r.entity_id > $3) \
               AND r.entity_id <= $4 \
             ORDER BY r.entity_id \
             LIMIT $5",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(after_entity_id)
        .bind(through_entity_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))?;

        Ok(rows
            .into_iter()
            .map(
                |(entity_id, is_live)| temper_runtime::persistence::KeyReconciliationEntity {
                    entity_id,
                    is_live,
                },
            )
            .collect())
    }

    async fn key_reconciliation_boundary(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Option<String>, PersistenceError> {
        crate::dbm::postgres_query_scalar!(
            "SELECT MAX(entity_id) FROM ( \
               SELECT DISTINCT e.entity_id FROM events e WHERE e.tenant = $1 AND e.entity_type = $2 \
               UNION SELECT DISTINCT k.entity_id FROM entity_key_index k WHERE k.tenant = $1 AND k.entity_type = $2 \
               UNION SELECT c.entity_id FROM entity_catalog c WHERE c.tenant = $1 AND c.entity_type = $2 \
               UNION SELECT DISTINCT f.entity_id FROM entity_field_index f WHERE f.tenant = $1 AND f.entity_type = $2 \
               UNION SELECT s.entity_id FROM snapshots s WHERE s.tenant = $1 AND s.entity_type = $2 \
             ) repair_ids",
        )
        .bind(tenant)
        .bind(entity_type)
        .fetch_one(&self.pool)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))
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
        let limit = limit.min(i64::MAX as usize) as i64;
        if let Some(entity_type) = entity_type {
            let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(
                "SELECT DISTINCT entity_type, entity_id \
                 FROM events \
                 WHERE tenant = $1 AND entity_type = $2 \
                 ORDER BY entity_type, entity_id \
                 LIMIT $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            return Ok(rows);
        }

        let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(
            "SELECT DISTINCT entity_type, entity_id \
             FROM events \
             WHERE tenant = $1 \
             ORDER BY entity_type, entity_id \
             LIMIT $2",
        )
        .bind(tenant)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "store_projection_test.rs"]
mod projection_tests;
