//! PostgreSQL-backed implementation of the [`EventStore`] trait.
//!
//! The store uses a `sqlx::PgPool` for all database access and relies on the
//! `UNIQUE (entity_type, entity_id, sequence_nr)` constraint to enforce
//! optimistic concurrency on appends.

use std::time::Instant;

use sqlx::{Acquire, PgPool};
use temper_runtime::persistence::{
    EntityVectorCandidate, EntityVectorRow, EventMetadata, EventStore, PersistenceAppend,
    PersistenceAppendResult, PersistenceEnvelope, PersistenceError, PersistenceSequenceGuard,
    contains_deletion_tombstone, ends_in_deletion_tombstone, pack_f32_le, unpack_f32_le,
    validate_guarded_persistence_append_batch,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use crate::metrics::{
    PostgresTransactionTimer, record_postgres_pool_acquire_duration,
    record_postgres_transaction_begin_duration, record_postgres_transaction_commit_duration,
};
use crate::segments;

const EVENT_APPEND_OPERATION: &str = "event_append";

fn postgres_sequence(sequence_nr: u64, operation: &str) -> Result<i64, PersistenceError> {
    i64::try_from(sequence_nr).map_err(|_| {
        PersistenceError::Storage(format!(
            "event sequence exceeds PostgreSQL range during {operation}"
        ))
    })
}

fn persisted_sequence(sequence_nr: i64, persistence_id: &str) -> Result<u64, PersistenceError> {
    u64::try_from(sequence_nr).map_err(|_| {
        PersistenceError::Serialization(format!("journal {persistence_id} has a negative sequence"))
    })
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

    async fn read_events_with_limit(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        limit: i64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        debug_assert!(limit >= 0, "event read limit must be non-negative");
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let stored_from_sequence = postgres_sequence(from_sequence, "event read")?;

        let rows: Vec<(i64, String, serde_json::Value, serde_json::Value)> =
            crate::dbm::postgres_query_as!(
                "SELECT sequence_nr, event_type, payload, metadata \
                 FROM events \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr > $4 \
                 ORDER BY sequence_nr ASC \
                 LIMIT $5",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(stored_from_sequence)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        rows.into_iter()
            .map(|(seq, event_type, payload, meta_json)| {
                let metadata: EventMetadata = serde_json::from_value(meta_json)
                    .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
                Ok(PersistenceEnvelope {
                    sequence_nr: persisted_sequence(seq, persistence_id)?,
                    event_type,
                    payload,
                    metadata,
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// EventStore implementation
// ---------------------------------------------------------------------------

impl PostgresEventStore {
    async fn append_with_indexes(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: Option<&[temper_runtime::persistence::EntityKeyRow]>,
        vector_rows: &[EntityVectorRow],
        reconcile_vectors: bool,
    ) -> Result<u64, PersistenceError> {
        if events.is_empty() {
            return Ok(expected_sequence);
        }
        postgres_sequence(expected_sequence, "append precondition")?;

        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let contains_deletion = contains_deletion_tombstone(events);
        let ends_in_deletion = ends_in_deletion_tombstone(events);
        let replaces_keys = key_rows.is_some();
        let key_rows = key_rows.unwrap_or_default();

        let mut transaction_timer = PostgresTransactionTimer::start(EVENT_APPEND_OPERATION);
        let acquire_started = Instant::now();
        let mut conn = match self.pool.acquire().await {
            Ok(conn) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    EVENT_APPEND_OPERATION,
                    "ok",
                );
                conn
            }
            Err(e) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    EVENT_APPEND_OPERATION,
                    "error",
                );
                return Err(PersistenceError::Storage(e.to_string()));
            }
        };
        let begin_started = Instant::now();
        let mut tx = match conn.begin().await {
            Ok(tx) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    EVENT_APPEND_OPERATION,
                    "ok",
                );
                tx
            }
            Err(e) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    EVENT_APPEND_OPERATION,
                    "error",
                );
                return Err(PersistenceError::Storage(e.to_string()));
            }
        };

        let row: Option<(i64,)> = crate::dbm::postgres_query_as!(
            "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        let current_seq = row
            .map(|row| persisted_sequence(row.0, persistence_id))
            .transpose()?
            .unwrap_or(0);
        if current_seq != expected_sequence {
            transaction_timer.set_outcome("concurrency_violation");
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: current_seq,
            });
        }

        // ADR-0153: validate declared-key uniqueness BEFORE writing the journal so
        // a reject is atomic (the journal does not advance). A different entity
        // already holding the key is the violation (reject + surface).
        for key in key_rows.iter().filter(|_| !ends_in_deletion) {
            let holder: Option<(String,)> = crate::dbm::postgres_query_as!(
                "SELECT entity_id FROM entity_key_index \
                 WHERE tenant = $1 AND entity_type = $2 AND key_name = $3 AND key_hash = $4",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(&key.key_name)
            .bind(&key.key_hash)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            if let Some((existing,)) = holder
                && existing != entity_id
            {
                return Err(PersistenceError::Storage(format!(
                    "duplicate declared key '{}' for {entity_type}: held by {existing}",
                    key.key_name
                )));
            }
        }

        let segment_index =
            segments::open_segment_for_append(&mut tx, tenant, entity_type, entity_id, current_seq)
                .await?;

        let mut new_seq = expected_sequence;
        for event in events {
            new_seq = new_seq.checked_add(1).ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "event sequence exhausted while appending {persistence_id}"
                ))
            })?;
            let stored_sequence = postgres_sequence(new_seq, "event append")?;
            let metadata_json = serde_json::to_value(&event.metadata)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;

            if let Err(e) = crate::dbm::postgres_query!(
                "INSERT INTO events (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(stored_sequence)
            .bind(segment_index)
            .bind(&event.event_type)
            .bind(&event.payload)
            .bind(metadata_json)
            .execute(&mut *tx)
            .await
            {
                let msg = e.to_string();
                if msg.contains("unique") || msg.contains("duplicate key") {
                    transaction_timer.set_outcome("concurrency_violation");
                    return Err(PersistenceError::ConcurrencyViolation {
                        expected: expected_sequence,
                        actual: new_seq,
                    });
                } else {
                    return Err(PersistenceError::Storage(msg));
                }
            }
        }

        if new_seq > expected_sequence {
            segments::update_segment_after_append(
                &mut tx,
                tenant,
                entity_type,
                entity_id,
                segment_index,
                new_seq,
            )
            .await?;
        }

        if contains_deletion || replaces_keys {
            // `Some(key_rows)` is the complete desired set after this append.
            // Raw `append` uses `None` and preserves existing claims because it
            // has no post-transition field state from which to rebuild them.
            // Tombstones always clear claims regardless of append mode.
            crate::dbm::postgres_query!(
                "DELETE FROM entity_key_index \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

            // ADR-0153: co-commit the complete declared key row set in THIS
            // transaction with the journal append.
            for key in key_rows.iter().filter(|_| !ends_in_deletion) {
                crate::dbm::postgres_query!(
                    "INSERT INTO entity_key_index \
                     (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr) \
                     VALUES ($1, $2, $3, $4, $5, $6)",
                )
                .bind(tenant)
                .bind(entity_type)
                .bind(&key.key_name)
                .bind(&key.key_hash)
                .bind(entity_id)
                .bind(postgres_sequence(new_seq, "declared-key append")?)
                .execute(&mut *tx)
                .await
                .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            }
        }

        // ADR-0155: co-commit the derived vector-index rows in THIS transaction.
        // When the entity's type declares vector paths (`reconcile_vectors`), DELETE
        // all of the entity's rows first, then insert the current ones — so a delete
        // transition or a cleared vector/model property (empty `vector_rows`) purges
        // the stale rows instead of leaving them to rank forever. No uniqueness
        // constraint; vectors are derived ranking state.
        if reconcile_vectors {
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
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(tenant)
                .bind(entity_type)
                .bind(&row.decl_name)
                .bind(&row.model_tag)
                .bind(entity_id)
                .bind(pack_f32_le(&row.vector))
                .bind(new_seq as i64)
                .execute(&mut *tx)
                .await
                .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            }
        }

        let commit_started = Instant::now();
        tx.commit().await.map_err(|e| {
            record_postgres_transaction_commit_duration(
                commit_started.elapsed(),
                EVENT_APPEND_OPERATION,
                "error",
            );
            PersistenceError::Storage(e.to_string())
        })?;
        record_postgres_transaction_commit_duration(
            commit_started.elapsed(),
            EVENT_APPEND_OPERATION,
            "ok",
        );
        transaction_timer.set_outcome("ok");

        Ok(new_seq)
    }
}

impl EventStore for PostgresEventStore {
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
        self.append_with_optional_keys(persistence_id, expected_sequence, events, None)
            .await
    }

    async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconcile_vectors: bool,
    ) -> Result<u64, PersistenceError> {
        self.append_with_indexes(
            persistence_id,
            expected_sequence,
            events,
            Some(key_rows),
            vector_rows,
            reconcile_vectors,
        )
        .await
    }

    async fn append_with_optional_keys(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: Option<&[temper_runtime::persistence::EntityKeyRow]>,
    ) -> Result<u64, PersistenceError> {
        self.append_with_indexes(
            persistence_id,
            expected_sequence,
            events,
            key_rows,
            &[],
            false,
        )
        .await
    }

    async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        for key in key_rows {
            // A different entity already holding this key is a pre-existing data
            // conflict. Fail this entity so the caller withholds the authoritative
            // watermark; silently skipping would bless an ambiguous index.
            let holder: Option<(String,)> = crate::dbm::postgres_query_as!(
                "SELECT entity_id FROM entity_key_index \
                 WHERE tenant = $1 AND entity_type = $2 AND key_name = $3 AND key_hash = $4",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(&key.key_name)
            .bind(&key.key_hash)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            if let Some((existing,)) = &holder
                && existing != entity_id
            {
                return Err(PersistenceError::Storage(format!(
                    "declared-key conflict for '{}': {entity_type}('{entity_id}') conflicts with '{existing}'",
                    key.key_name
                )));
            }
        }
        crate::dbm::postgres_query!(
            "DELETE FROM entity_key_index \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        for key in key_rows {
            crate::dbm::postgres_query!(
                "INSERT INTO entity_key_index \
                 (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr) \
                 VALUES ($1, $2, $3, $4, $5, 0)",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(&key.key_name)
            .bind(&key.key_hash)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        }
        tx.commit()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn retire_entity_keys_through_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        delete_sequence: u64,
    ) -> Result<(), PersistenceError> {
        let delete_sequence = i64::try_from(delete_sequence).map_err(|_| {
            PersistenceError::Storage("delete sequence exceeds PostgreSQL range".to_string())
        })?;
        crate::dbm::postgres_query!(
            "DELETE FROM entity_key_index k \
             WHERE k.tenant = $1 AND k.entity_type = $2 AND k.entity_id = $3 \
               AND k.sequence_nr <= $4 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM events e \
                    WHERE e.tenant = k.tenant \
                      AND e.entity_type = k.entity_type \
                      AND e.entity_id = k.entity_id \
                      AND e.sequence_nr > $4 \
                      AND e.event_type = 'Created' \
               )",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(delete_sequence)
        .execute(&self.pool)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        Ok(())
    }

    async fn mark_key_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<(), PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        // Upsert the covered key-set: a re-key after a key-set change must OVERWRITE the
        // stale set (not DO NOTHING) so `complete` reflects the keys actually assigned.
        crate::dbm::postgres_query!(
            "INSERT INTO key_index_backfill_watermark (tenant, entity_type, key_set) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (tenant, entity_type) \
             DO UPDATE SET key_set = EXCLUDED.key_set, completed_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(key_set)
        .execute(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(
            "SELECT entity_type, key_set FROM key_index_backfill_watermark WHERE tenant = $1",
        )
        .bind(tenant)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(rows.into_iter().collect())
    }

    async fn keyed_entity_ids_for_type(
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
            "SELECT DISTINCT entity_id FROM entity_key_index \
             WHERE tenant = $1 AND entity_type = $2",
        )
        .bind(tenant)
        .bind(entity_type)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(rows.into_iter().map(|(entity_id,)| entity_id).collect())
    }

    async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        let row: Option<(String,)> = crate::dbm::postgres_query_as!(
            "SELECT entity_id FROM entity_key_index \
             WHERE tenant = $1 AND entity_type = $2 AND key_name = $3 AND key_hash = $4",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(key_name)
        .bind(key_hash)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        Ok(row.map(|(id,)| id))
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

    /// Atomically append to multiple entity journals in one PostgreSQL
    /// transaction. Used as the storage foundation for cross-actor Composite
    /// transactions: every stream's optimistic-concurrency check must pass
    /// before any event is inserted.
    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        EventStore::append_batch_guarded(self, appends, &[]).await
    }

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

        let mut transaction_timer = PostgresTransactionTimer::start(EVENT_APPEND_OPERATION);
        let acquire_started = Instant::now();
        let mut conn = match self.pool.acquire().await {
            Ok(conn) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    EVENT_APPEND_OPERATION,
                    "ok",
                );
                conn
            }
            Err(e) => {
                record_postgres_pool_acquire_duration(
                    acquire_started.elapsed(),
                    EVENT_APPEND_OPERATION,
                    "error",
                );
                return Err(PersistenceError::Storage(e.to_string()));
            }
        };
        let begin_started = Instant::now();
        let mut tx = match conn.begin().await {
            Ok(tx) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    EVENT_APPEND_OPERATION,
                    "ok",
                );
                tx
            }
            Err(e) => {
                record_postgres_transaction_begin_duration(
                    begin_started.elapsed(),
                    EVENT_APPEND_OPERATION,
                    "error",
                );
                return Err(PersistenceError::Storage(e.to_string()));
            }
        };

        if !guards.is_empty() {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
                .execute(&mut *tx)
                .await
                .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        }

        for guard in guards {
            postgres_sequence(guard.expected_sequence, "guarded append precondition")?;
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&guard.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            let row: Option<(i64,)> = crate::dbm::postgres_query_as!(
                "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;
            let actual = row
                .map(|row| persisted_sequence(row.0, &guard.persistence_id))
                .transpose()?
                .unwrap_or(0);
            if actual != guard.expected_sequence {
                transaction_timer.set_outcome("precondition_failed");
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
            postgres_sequence(append.expected_sequence, "batch append precondition")?;
            let row: Option<(i64,)> = crate::dbm::postgres_query_as!(
                "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

            let current_seq = row
                .map(|row| persisted_sequence(row.0, &append.persistence_id))
                .transpose()?
                .unwrap_or(0);
            if current_seq != append.expected_sequence {
                transaction_timer.set_outcome("concurrency_violation");
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: append.expected_sequence,
                    actual: current_seq,
                });
            }
            let segment_index = Some(
                segments::open_segment_for_append(
                    &mut tx,
                    tenant,
                    entity_type,
                    entity_id,
                    current_seq,
                )
                .await?,
            );
            parsed.push((
                tenant.to_string(),
                entity_type.to_string(),
                entity_id.to_string(),
                segment_index,
            ));
        }

        // Replace claims only when the caller supplied an authoritative key
        // set. Raw appends preserve claims they cannot recompute; tombstones
        // always clear them. Removing all replacement sets before insertion
        // permits atomic key swaps inside one batch.
        for (append, (tenant, entity_type, entity_id, _)) in appends.iter().zip(parsed.iter()) {
            if append.events.is_empty()
                || (append.key_rows.is_none() && !contains_deletion_tombstone(&append.events))
            {
                continue;
            }
            crate::dbm::postgres_query!(
                "DELETE FROM entity_key_index \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        }

        // Vector rows are derived state, but when a caller has recomputed them
        // they must be co-committed with the same guarded/batch append as the
        // journal and declared-key rows. Otherwise a lifecycle guard could make
        // Postgres diverge from Sim/normal append behavior.
        for (append, (tenant, entity_type, entity_id, _)) in appends.iter().zip(parsed.iter()) {
            if append.events.is_empty() || !append.reconcile_vectors {
                continue;
            }
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
        }

        let mut results = Vec::with_capacity(appends.len());
        for (append, (tenant, entity_type, entity_id, segment_index)) in
            appends.iter().zip(parsed.iter())
        {
            let mut new_seq = append.expected_sequence;
            let segment_index = *segment_index;
            for event in &append.events {
                new_seq = new_seq.checked_add(1).ok_or_else(|| {
                    PersistenceError::Storage(format!(
                        "event sequence exhausted while appending {}",
                        append.persistence_id
                    ))
                })?;
                let stored_sequence = postgres_sequence(new_seq, "batch event append")?;
                let metadata_json = serde_json::to_value(&event.metadata)
                    .map_err(|e| PersistenceError::Serialization(e.to_string()))?;

                if let Err(e) = crate::dbm::postgres_query!(
                    "INSERT INTO events (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                )
                .bind(tenant)
                .bind(entity_type)
                .bind(entity_id)
                .bind(stored_sequence)
                .bind(segment_index.expect("non-empty append has an event segment"))
                .bind(&event.event_type)
                .bind(&event.payload)
                .bind(metadata_json)
                .execute(&mut *tx)
                .await
                {
                    let msg = e.to_string();
                    if msg.contains("unique") || msg.contains("duplicate key") {
                        transaction_timer.set_outcome("concurrency_violation");
                        return Err(PersistenceError::ConcurrencyViolation {
                            expected: append.expected_sequence,
                            actual: new_seq,
                        });
                    }
                    return Err(PersistenceError::Storage(msg));
                }
            }
            if new_seq > append.expected_sequence {
                segments::update_segment_after_append(
                    &mut tx,
                    tenant,
                    entity_type,
                    entity_id,
                    segment_index.expect("advanced append has an event segment"),
                    new_seq,
                )
                .await?;
            }
            if !ends_in_deletion_tombstone(&append.events) {
                for key in append.key_rows.as_deref().unwrap_or_default() {
                    crate::dbm::postgres_query!(
                        "INSERT INTO entity_key_index \
                         (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr) \
                         VALUES ($1, $2, $3, $4, $5, $6)",
                    )
                    .bind(tenant)
                    .bind(entity_type)
                    .bind(&key.key_name)
                    .bind(&key.key_hash)
                    .bind(entity_id)
                    .bind(postgres_sequence(new_seq, "batch declared-key append")?)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
                }
            }
            if append.reconcile_vectors {
                for row in &append.vector_rows {
                    crate::dbm::postgres_query!(
                        "INSERT INTO entity_vector_index \
                         (tenant, entity_type, decl_name, model_tag, entity_id, vector, sequence_nr) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    )
                    .bind(tenant)
                    .bind(entity_type)
                    .bind(&row.decl_name)
                    .bind(&row.model_tag)
                    .bind(entity_id)
                    .bind(pack_f32_le(&row.vector))
                    .bind(postgres_sequence(new_seq, "batch vector append")?)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
                }
            }
            results.push(PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr: new_seq,
            });
        }

        let commit_started = Instant::now();
        tx.commit().await.map_err(|e| {
            record_postgres_transaction_commit_duration(
                commit_started.elapsed(),
                EVENT_APPEND_OPERATION,
                "error",
            );
            PersistenceError::Storage(e.to_string())
        })?;
        record_postgres_transaction_commit_duration(
            commit_started.elapsed(),
            EVENT_APPEND_OPERATION,
            "ok",
        );
        transaction_timer.set_outcome("ok");

        Ok(results)
    }

    /// Read events from the journal starting after `from_sequence`.
    ///
    /// Events are returned in ascending `sequence_nr` order.
    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.read_events_with_limit(persistence_id, from_sequence, i64::MAX)
            .await
    }

    async fn read_events_bounded(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let limit = i64::try_from(limit).map_err(|_| {
            PersistenceError::Storage("event read limit exceeds PostgreSQL range".to_string())
        })?;
        self.read_events_with_limit(persistence_id, from_sequence, limit)
            .await
    }

    async fn read_latest_events(
        &self,
        persistence_ids: &[String],
    ) -> Result<Vec<Option<PersistenceEnvelope>>, PersistenceError> {
        crate::latest_events::read_latest_events(&self.pool, persistence_ids).await
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
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let stored_sequence = i64::try_from(sequence_nr).map_err(|_| {
            PersistenceError::Storage("snapshot sequence exceeds PostgreSQL range".to_string())
        })?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        crate::dbm::postgres_query!(
            "INSERT INTO snapshots (tenant, entity_type, entity_id, sequence_nr, state) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant, entity_type, entity_id) \
             DO UPDATE SET sequence_nr = $4, state = $5, created_at = now() \
             WHERE EXCLUDED.sequence_nr >= snapshots.sequence_nr",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(stored_sequence)
        .bind(snapshot)
        .execute(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        crate::dbm::postgres_query!(
            "INSERT INTO snapshot_history (tenant, entity_type, entity_id, sequence_nr, state) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant, entity_type, entity_id, sequence_nr) \
             DO UPDATE SET state = $5, created_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(stored_sequence)
        .bind(snapshot)
        .execute(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        segments::rotate_after_snapshot(&mut tx, tenant, entity_type, entity_id, sequence_nr)
            .await?;

        tx.commit()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(())
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

        row.map(|(sequence_nr, state)| {
            persisted_sequence(sequence_nr, persistence_id).map(|sequence_nr| (sequence_nr, state))
        })
        .transpose()
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
            "SELECT DISTINCT entity_id \
             FROM events \
             WHERE tenant = $1 AND entity_type = $2 \
             ORDER BY entity_id",
        )
        .bind(tenant)
        .bind(entity_type)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(rows)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::run_migrations;
    use temper_runtime::tenant::parse_persistence_id_parts;

    fn parse_pid(persistence_id: &str) -> Result<(&str, &str, &str), PersistenceError> {
        parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)
    }

    fn test_envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
        PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload,
            metadata: EventMetadata {
                event_id: uuid::Uuid::new_v4(),
                causation_id: uuid::Uuid::new_v4(),
                correlation_id: uuid::Uuid::new_v4(),
                timestamp: chrono::Utc::now(),
                actor_id: "store-test".to_string(),
            },
        }
    }

    #[test]
    fn sequence_conversions_reject_overflow_and_negative_storage() {
        assert_eq!(
            postgres_sequence(i64::MAX as u64, "test").unwrap(),
            i64::MAX
        );
        assert!(postgres_sequence(i64::MAX as u64 + 1, "test").is_err());
        assert!(persisted_sequence(-1, "tenant:Order:bad").is_err());
    }

    // -- persistence_id parsing ---------------------------------------------

    #[test]
    fn parse_3_segment_persistence_id() {
        let (tenant, entity_type, entity_id) =
            parse_pid("alpha:Order:abc-123").expect("valid three-segment persistence ID");
        assert_eq!(tenant, "alpha");
        assert_eq!(entity_type, "Order");
        assert_eq!(entity_id, "abc-123");
    }

    #[test]
    fn parse_legacy_2_segment_persistence_id() {
        let (tenant, entity_type, entity_id) =
            parse_pid("Order:abc-123").expect("valid legacy two-segment persistence ID");
        assert_eq!(tenant, "default");
        assert_eq!(entity_type, "Order");
        assert_eq!(entity_id, "abc-123");
    }

    #[test]
    fn parse_3_segment_with_colons_in_id() {
        // splitn(3, ':') puts everything after the second colon into entity_id
        let (tenant, entity_type, entity_id) =
            parse_pid("beta:Task:T-1:sub").expect("valid persistence ID with colon in entity ID");
        assert_eq!(tenant, "beta");
        assert_eq!(entity_type, "Task");
        assert_eq!(entity_id, "T-1:sub");
    }

    #[test]
    fn parse_persistence_id_missing_colon() {
        let err = parse_pid("OrderAbc123").unwrap_err();
        assert!(
            matches!(err, PersistenceError::Storage(_)),
            "expected Storage error, got: {err:?}"
        );
    }

    #[test]
    fn parse_persistence_id_empty_segment() {
        assert!(parse_pid(":Order:abc").is_err());
        assert!(parse_pid("tenant::abc").is_err());
        assert!(parse_pid("tenant:Order:").is_err());
        assert!(parse_pid(":abc").is_err());
        assert!(parse_pid("Order:").is_err());
    }

    #[test]
    fn list_entity_ids_returns_distinct_pairs() {
        let database_url = match std::env::var("DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!("skipping Postgres integration test: DATABASE_URL is not set");
                return;
            }
        };

        sqlx::test_block_on(async {
            let pool = PgPool::connect(&database_url)
                .await
                .expect("connect to DATABASE_URL");
            run_migrations(&pool).await.expect("run migrations");
            let store = PostgresEventStore::new(pool);

            let tenant_a = format!("tenant-a-{}", uuid::Uuid::new_v4());
            let tenant_b = format!("tenant-b-{}", uuid::Uuid::new_v4());

            let order_1 = format!("{tenant_a}:Order:ord-1");
            let order_2 = format!("{tenant_a}:Order:ord-2");
            let task_1 = format!("{tenant_a}:Task:task-1");
            let other_tenant = format!("{tenant_b}:Order:ord-9");

            store
                .append(
                    &order_1,
                    0,
                    &[test_envelope(
                        "OrderCreated",
                        serde_json::json!({"id":"ord-1"}),
                    )],
                )
                .await
                .expect("append first order event");
            store
                .append(
                    &order_1,
                    1,
                    &[test_envelope(
                        "OrderUpdated",
                        serde_json::json!({"step": 2}),
                    )],
                )
                .await
                .expect("append order update event");
            store
                .append(
                    &order_2,
                    0,
                    &[test_envelope(
                        "OrderCreated",
                        serde_json::json!({"id":"ord-2"}),
                    )],
                )
                .await
                .expect("append second order event");
            store
                .append(
                    &task_1,
                    0,
                    &[test_envelope(
                        "TaskCreated",
                        serde_json::json!({"id":"task-1"}),
                    )],
                )
                .await
                .expect("append task event");
            store
                .append(
                    &other_tenant,
                    0,
                    &[test_envelope(
                        "OrderCreated",
                        serde_json::json!({"id":"ord-9"}),
                    )],
                )
                .await
                .expect("append other tenant event");

            let mut entities = store
                .list_entity_ids(&tenant_a)
                .await
                .expect("list tenant entity IDs");
            entities.sort();

            assert_eq!(
                entities,
                vec![
                    ("Order".to_string(), "ord-1".to_string()),
                    ("Order".to_string(), "ord-2".to_string()),
                    ("Task".to_string(), "task-1".to_string()),
                ]
            );
        });
    }

    #[test]
    fn postgres_platform_methods_are_part_of_the_store_surface() {
        // Compile-only check: the function body is never executed, so the
        // unawaited futures are intentional. Clippy's let_underscore_future
        // lint catches forgotten awaits in real code; here it's a false
        // positive.
        #[allow(clippy::let_underscore_future)]
        fn assert_methods(store: &PostgresEventStore) {
            let _ = store.upsert_query_projection(
                "tenant",
                "Session",
                "s-1",
                "Running",
                &serde_json::json!({"phase":"Running"}),
                1,
            );
            let _ = store.remove_query_projection("tenant", "Session", "s-1");
            let _ = store.query_field_index(
                "tenant",
                "Session",
                "fields @> $3::jsonb",
                vec!["{\"phase\":\"Running\"}".to_string()],
            );
            let _ = store.persist_trajectory(crate::PostgresTrajectoryInsert {
                tenant: "tenant",
                entity_type: "Session",
                entity_id: "s-1",
                action: "ProgressMade",
                success: true,
                from_status: Some("Running"),
                to_status: Some("Running"),
                error: None,
                agent_id: Some("agent"),
                session_id: Some("session"),
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
                source: Some("Entity"),
                spec_governed: Some(true),
                created_at: "2026-04-28T00:00:00Z",
                request_body: Some("{\"ok\":true}"),
                intent: Some("test"),
                matched_policy_ids: Some("[\"policy:test\"]"),
            });
            let _ = store.save_policy(
                "tenant",
                "primary",
                "permit(principal, action, resource);",
                "test",
            );
            let _ = store.load_policies_for_tenant("tenant");
            let _ = store.load_all_policies();
            let _ = store.toggle_policy_enabled("tenant", "primary", true);
            let _ = store.update_policy_text(
                "tenant",
                "primary",
                "permit(principal, action, resource);",
                "test",
            );
            let _ = store.delete_policy("tenant", "primary");
        }

        let _ = assert_methods;
    }

    #[test]
    fn postgres_long_tail_methods_are_part_of_the_store_surface() {
        let _ = PostgresEventStore::load_recent_trajectories;
        let _ = PostgresEventStore::load_unmet_intent_rows;
        let _ = PostgresEventStore::load_submit_spec_timestamps;
        let _ = PostgresEventStore::count_trajectories_by_tenant;
        let _ = PostgresEventStore::query_trajectory_stats;
        let _ = PostgresEventStore::query_trajectories_by_agent;
        let _ = PostgresEventStore::query_agent_summaries;

        let _ = PostgresEventStore::upsert_feature_request;
        let _ = PostgresEventStore::list_feature_requests;
        let _ = PostgresEventStore::update_feature_request;
        let _ = PostgresEventStore::insert_evolution_record;
        let _ = PostgresEventStore::get_evolution_record;
        let _ = PostgresEventStore::list_evolution_records;
        let _ = PostgresEventStore::list_ranked_insights;
        let _ = PostgresEventStore::insert_design_time_event;
        let _ = PostgresEventStore::list_design_time_events;

        let _ = PostgresEventStore::persist_ots_trajectory;
        let _ = PostgresEventStore::list_ots_trajectories;
        let _ = PostgresEventStore::get_ots_trajectory;

        let _ = PostgresEventStore::put_blob;
        let _ = PostgresEventStore::put_blob_with_ttl;
        let _ = PostgresEventStore::get_blob;
        let _ = PostgresEventStore::sweep_expired_blobs;

        let _ = PostgresEventStore::upsert_secret;
        let _ = PostgresEventStore::delete_secret;
        let _ = PostgresEventStore::load_secrets_for_tenant;

        let _ = PostgresEventStore::upsert_policy_denial_pattern;
        let _ = PostgresEventStore::load_policy_denial_patterns;

        let _ = PostgresEventStore::query_decisions;
        let _ = PostgresEventStore::query_all_decisions;
        let _ = PostgresEventStore::get_pending_decision;

        let _ = PostgresEventStore::load_wasm_module;
        let _ = PostgresEventStore::load_wasm_module_metadata_all_tenants;
        let _ = PostgresEventStore::persist_wasm_invocation;
        let _ = PostgresEventStore::load_recent_wasm_invocations;
        let _ = PostgresEventStore::delete_wasm_module;

        let _ = PostgresEventStore::upsert_published_artifact;
        let _ = PostgresEventStore::load_published_artifact;
    }

    #[test]
    fn postgres_query_projection_batch_method_is_part_of_the_store_surface() {
        let _ = PostgresEventStore::load_query_projection_fields_many;
        let _ = PostgresEventStore::load_selected_entity_catalog_rows_pg;
    }

    #[test]
    fn load_query_projection_fields_many_returns_requested_fields() {
        let database_url = match std::env::var("DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!("skipping Postgres integration test: DATABASE_URL is not set");
                return;
            }
        };

        sqlx::test_block_on(async {
            let pool = PgPool::connect(&database_url)
                .await
                .expect("connect to DATABASE_URL");
            run_migrations(&pool).await.expect("run migrations");
            let store = PostgresEventStore::new(pool.clone());
            let tenant = format!("tenant-projection-{}", uuid::Uuid::new_v4());

            store
                .upsert_query_projection(
                    &tenant,
                    "File",
                    "file-a",
                    "Ready",
                    &serde_json::json!({
                        "content_hash": "sha256:file-a",
                        "mime_type": "text/plain",
                        "has_content": true,
                    }),
                    12,
                )
                .await
                .expect("upsert query projection row");

            let rows = store
                .load_query_projection_fields_many(
                    &tenant,
                    "File",
                    &["file-a".to_string(), "missing".to_string()],
                    &["content_hash", "has_content"],
                )
                .await
                .expect("load requested projection fields");

            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].entity_id, "file-a");
            assert_eq!(rows[0].status, "Ready");
            assert_eq!(
                rows[0]
                    .fields
                    .get("content_hash")
                    .and_then(|value| value.as_deref()),
                Some("sha256:file-a")
            );
            assert_eq!(
                rows[0]
                    .fields
                    .get("has_content")
                    .and_then(|value| value.as_deref()),
                Some("true")
            );

            crate::dbm::postgres_query!("DELETE FROM entity_field_index WHERE tenant = $1")
                .bind(&tenant)
                .execute(&pool)
                .await
                .expect("delete test entity field index rows");
            crate::dbm::postgres_query!("DELETE FROM entity_catalog WHERE tenant = $1")
                .bind(&tenant)
                .execute(&pool)
                .await
                .expect("delete test entity catalog rows");
        });
    }

    #[test]
    fn published_artifact_upsert_round_trips_and_updates_by_id() {
        let database_url = match std::env::var("DATABASE_URL") {
            Ok(url) => url,
            Err(_) => return,
        };

        sqlx::test_block_on(async {
            let pool = PgPool::connect(&database_url)
                .await
                .expect("connect to DATABASE_URL");
            run_migrations(&pool).await.expect("run migrations");
            let store = PostgresEventStore::new(pool.clone());
            let tenant = format!("tenant-published-artifact-{}", uuid::Uuid::new_v4());

            let artifact = crate::PostgresPublishedArtifactUpsert {
                id: "part-test",
                tenant: &tenant,
                source_file_id: "file-a",
                source_file_version_id: "",
                content_hash: "sha256:abc",
                label: "latest",
                mime_type: "text/markdown",
                byte_length: 42,
                public_storage_key: "published/file-a.md",
                public_url: "https://assets.example/published/file-a.md",
                owner_ref_type: "Doc",
                owner_ref_id: "doc-a",
                status: "published",
            };

            let row = store
                .upsert_published_artifact(&artifact)
                .await
                .expect("upsert published artifact");
            assert_eq!(row.tenant, tenant);
            assert_eq!(row.id, "part-test");
            assert_eq!(row.public_url, "https://assets.example/published/file-a.md");

            let updated = crate::PostgresPublishedArtifactUpsert {
                public_url: "https://assets.example/published/file-a-v2.md",
                public_storage_key: "published/file-a-v2.md",
                byte_length: 43,
                ..artifact
            };
            store
                .upsert_published_artifact(&updated)
                .await
                .expect("update published artifact");

            let loaded = store
                .load_published_artifact(&tenant, "part-test")
                .await
                .expect("load published artifact")
                .expect("artifact should load after upsert");

            assert_eq!(
                loaded.public_url,
                "https://assets.example/published/file-a-v2.md"
            );
            assert_eq!(loaded.public_storage_key, "published/file-a-v2.md");
            assert_eq!(loaded.byte_length, 43);

            crate::dbm::postgres_query!("DELETE FROM published_artifacts WHERE tenant = $1")
                .bind(&tenant)
                .execute(&pool)
                .await
                .expect("delete test published artifact rows");
        });
    }
}
