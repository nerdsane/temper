//! PostgreSQL-backed implementation of the [`EventStore`] trait.
//!
//! The store uses a `sqlx::PgPool` for all database access and relies on the
//! `UNIQUE (entity_type, entity_id, sequence_nr)` constraint to enforce
//! optimistic concurrency on appends.

use std::time::Instant;

use sqlx::{Acquire, PgPool};
use temper_runtime::persistence::{
    CommitRows, EntityVectorCandidate, EntityVectorRow, EventMetadata, EventStore,
    PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
    ScheduledActionRecord, pack_f32_le, unpack_f32_le,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use crate::metrics::{
    PostgresTransactionTimer, record_postgres_pool_acquire_duration,
    record_postgres_transaction_begin_duration, record_postgres_transaction_commit_duration,
};
use crate::schema::tombstone_sql;
use crate::segments;

const EVENT_APPEND_OPERATION: &str = "event_append";

/// A PostgreSQL-backed event store.
///
/// Persistence IDs follow `"tenant:entity_type:entity_id"` (with legacy
/// `"entity_type:entity_id"` mapped to tenant `"default"`). Components are
/// stored in separate columns for efficient filtering.
#[derive(Clone, Debug)]
pub struct PostgresEventStore {
    pool: PgPool,
    pub(crate) schema: crate::PostgresSchema,
}

impl PostgresEventStore {
    /// Create a new store backed by the given connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            schema: crate::PostgresSchema::default(),
        }
    }

    /// Create a store that explicitly addresses tables in `schema`.
    /// This never changes connection settings or creates database objects.
    pub fn with_schema(pool: PgPool, schema: crate::PostgresSchema) -> Self {
        Self { pool, schema }
    }

    /// Schema selected for this store; the default follows the connection's search path.
    pub fn schema(&self) -> &crate::PostgresSchema {
        &self.schema
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
        self.append_with_rows(
            persistence_id,
            expected_sequence,
            events,
            CommitRows::default(),
        )
        .await
    }

    /// Append `events` and every row in `rows` in one statement
    /// (`append_commit`): the journal, its keys, vectors and scheduled actions,
    /// and the record's catalog, field-index, timer and gap rows.
    async fn append_with_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        rows: CommitRows<'_>,
    ) -> Result<u64, PersistenceError> {
        self.append_commit(persistence_id, expected_sequence, events, rows)
            .await
    }

    /// The append statement writes the record's query-plane and timer rows.
    fn commits_entity_rows(&self) -> bool {
        true
    }

    async fn list_scheduled_actions(
        &self,
        tenant: &str,
    ) -> Result<Vec<ScheduledActionRecord>, PersistenceError> {
        self.scheduled_action_rows(tenant).await
    }

    async fn delete_scheduled_action(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.remove_scheduled_action(tenant, entity_type, entity_id, arm_id)
            .await
    }

    async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        if key_rows.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
        for key in key_rows {
            // A different entity already holding this key is a pre-existing data
            // conflict — log and skip (don't fail the whole backfill on one row;
            // the conflict surfaces via the metric and a keyed read still resolves
            // to whoever currently holds it).
            let holder: Option<(String,)> = crate::dbm::postgres_query_as!(
                &self.schema,
                "SELECT entity_id FROM {schema}entity_key_index \
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
                tracing::warn!(
                    tenant, entity_type, entity_id, existing,
                    key_name = %key.key_name,
                    "entity_key_index backfill: declared-key conflict; skipping"
                );
                continue;
            }
            crate::dbm::postgres_query!(
                &self.schema,
                "DELETE FROM {schema}entity_key_index \
                 WHERE tenant = $1 AND entity_type = $2 AND key_name = $3 AND entity_id = $4",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(&key.key_name)
            .bind(entity_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            crate::dbm::postgres_query!(
                &self.schema,
                "INSERT INTO {schema}entity_key_index \
                 (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr) \
                 VALUES ($1, $2, $3, $4, $5, 0) \
                 ON CONFLICT (tenant, entity_type, key_name, key_hash) DO NOTHING",
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
            &self.schema,
            "INSERT INTO {schema}key_index_backfill_watermark (tenant, entity_type, key_set) \
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
        let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(&self.schema,
            "SELECT entity_type, key_set FROM {schema}key_index_backfill_watermark WHERE tenant = $1",
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
            &self.schema,
            "SELECT DISTINCT entity_id FROM {schema}entity_key_index \
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
            &self.schema,
            "SELECT entity_id FROM {schema}entity_key_index \
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
            &self.schema,
            "DELETE FROM {schema}entity_vector_index \
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
                &self.schema,
                "INSERT INTO {schema}entity_vector_index \
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
            &self.schema,
            "SELECT entity_id, vector FROM {schema}entity_vector_index \
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
            &self.schema,
            "INSERT INTO {schema}vector_index_backfill_watermark (tenant, entity_type, vector_set) \
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
        let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(&self.schema,
            "SELECT entity_type, vector_set FROM {schema}vector_index_backfill_watermark WHERE tenant = $1",
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
            &self.schema,
            "SELECT DISTINCT entity_id FROM {schema}entity_vector_index \
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
        if appends.is_empty() {
            return Ok(Vec::new());
        }

        let mut seen = std::collections::BTreeSet::new();
        for append in appends {
            if !seen.insert(append.persistence_id.as_str()) {
                return Err(PersistenceError::Storage(format!(
                    "duplicate persistence_id '{}' in append_batch",
                    append.persistence_id
                )));
            }
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

        // Each record's events and rows go in through the single-append
        // statement, so a batch writes exactly what the same appends would
        // one by one, and its transaction commits them together. The records
        // are taken in persistence-id order, so two batches over the same
        // records take their locks in the same order.
        let mut order: Vec<usize> = (0..appends.len()).collect();
        order.sort_by(|a, b| appends[*a].persistence_id.cmp(&appends[*b].persistence_id));
        let mut written = vec![0_u64; appends.len()];
        for index in order {
            let append = &appends[index];
            written[index] = self
                .append_on(
                    crate::append_commit::AppendOn::InBatch(&mut tx),
                    &append.persistence_id,
                    append.expected_sequence,
                    &append.events,
                    append.commit_rows(),
                    &mut transaction_timer,
                )
                .await?;
        }
        let results: Vec<PersistenceAppendResult> = appends
            .iter()
            .zip(written)
            .map(|(append, sequence_nr)| PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr,
            })
            .collect();

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
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;

        let rows: Vec<(i64, String, serde_json::Value, serde_json::Value)> =
            crate::dbm::postgres_query_as!(
                &self.schema,
                "SELECT sequence_nr, event_type, payload, metadata \
             FROM {schema}events \
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

        rows.into_iter().map(envelope_from_row).collect()
    }

    /// Read the newest `limit` events below `before_sequence`, ascending.
    ///
    /// Reads newest first along the `(tenant, entity_type, entity_id,
    /// sequence_nr)` key, so only the page's rows are read.
    async fn read_events_page(
        &self,
        persistence_id: &str,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        // Both bounds saturate: a sequence or a row count above i64::MAX is
        // past every stored row, so it means "no bound".
        let before = before_sequence.map_or(i64::MAX, |s| i64::try_from(s).unwrap_or(i64::MAX));
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);

        let rows: Vec<(i64, String, serde_json::Value, serde_json::Value)> =
            crate::dbm::postgres_query_as!(
                &self.schema,
                "SELECT sequence_nr, event_type, payload, metadata \
             FROM {schema}events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr < $4 \
             ORDER BY sequence_nr DESC \
             LIMIT $5",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(before)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        let mut page = rows
            .into_iter()
            .map(envelope_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        page.reverse();
        Ok(page)
    }

    async fn first_event_at(
        &self,
        persistence_id: &str,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let saved: Option<String> = crate::dbm::postgres_query_scalar!(&self.schema,
            "SELECT COALESCE(
                (SELECT first_event_at FROM {schema}entity_catalog WHERE tenant=$1 AND entity_type=$2 AND entity_id=$3),
                (SELECT COALESCE(payload->>'timestamp', metadata->>'timestamp') FROM {schema}events
                 WHERE tenant=$1 AND entity_type=$2 AND entity_id=$3 AND sequence_nr=1))")
            .bind(tenant).bind(entity_type).bind(entity_id).fetch_one(self.pool()).await.map_err(|error| PersistenceError::Storage(error.to_string()))?;
        saved
            .map(|saved| {
                chrono::DateTime::parse_from_rfc3339(&saved)
                    .map(|t| t.with_timezone(&chrono::Utc))
                    .map_err(|error| PersistenceError::Serialization(error.to_string()))
            })
            .transpose()
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

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        crate::dbm::postgres_query!(
            &self.schema,
            "INSERT INTO {schema}snapshots (tenant, entity_type, entity_id, sequence_nr, state) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant, entity_type, entity_id) \
             DO UPDATE SET sequence_nr = $4, state = $5, created_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(sequence_nr as i64)
        .bind(snapshot)
        .execute(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        segments::rotate_after_snapshot(
            &self.schema,
            &mut tx,
            tenant,
            entity_type,
            entity_id,
            sequence_nr,
        )
        .await?;
        crate::projection_delta::mark_snapshot(
            &self.schema,
            &mut tx,
            [tenant, entity_type, entity_id],
            sequence_nr,
        )
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
            &self.schema,
            "SELECT sequence_nr, state FROM {schema}snapshots \
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

    async fn latest_sequence_nr(&self, persistence_id: &str) -> Result<u64, PersistenceError> {
        let (tenant, ty, id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        self.journal_sequence_nr(tenant, ty, id).await
    }

    async fn journal_heads(
        &self,
        tenant: &str,
    ) -> Result<Vec<temper_runtime::persistence::JournalHead>, PersistenceError> {
        self.load_journal_heads(tenant).await
    }

    /// List distinct records with events in this tenant.
    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let rows: Vec<(String, String)> = crate::dbm::postgres_query_as!(
            &self.schema,
            "SELECT DISTINCT entity_type, entity_id \
             FROM {schema}events \
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
            &self.schema,
            concat!(
                "SELECT entity_id \
             FROM ( \
               SELECT c.entity_id \
               FROM {schema}entity_catalog c \
               WHERE c.tenant = $1 \
                 AND c.entity_type = $2 \
                 AND NOT EXISTS ( \
                   SELECT 1 \
                   FROM {schema}events d \
                   WHERE d.tenant = c.tenant \
                     AND d.entity_type = c.entity_type \
                     AND d.entity_id = c.entity_id \
                     AND ",
                tombstone_sql!(),
                "
                 ) \
               UNION \
               SELECT f.entity_id \
               FROM {schema}entity_field_index f \
               WHERE f.tenant = $1 \
                 AND f.entity_type = $2 \
                 AND NOT EXISTS ( \
                   SELECT 1 \
                   FROM {schema}events d \
                   WHERE d.tenant = f.tenant \
                     AND d.entity_type = f.entity_type \
                     AND d.entity_id = f.entity_id \
                     AND ",
                tombstone_sql!(),
                "
                 ) \
               UNION \
               SELECT DISTINCT e.entity_id \
               FROM {schema}events e \
               WHERE e.tenant = $1 \
                 AND e.entity_type = $2 \
                 AND NOT EXISTS ( \
                   SELECT 1 \
                   FROM {schema}events d \
                   WHERE d.tenant = e.tenant \
                     AND d.entity_type = e.entity_type \
                     AND d.entity_id = e.entity_id \
                     AND ",
                tombstone_sql!(),
                "
                 ) \
             ) ids \
             ORDER BY entity_id"
            ),
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
                &self.schema,
                "SELECT DISTINCT entity_type, entity_id \
                 FROM {schema}events \
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
            &self.schema,
            "SELECT DISTINCT entity_type, entity_id \
             FROM {schema}events \
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

/// Decode one `SELECT sequence_nr, event_type, payload, metadata` row.
fn envelope_from_row(
    (seq, event_type, payload, meta_json): (i64, String, serde_json::Value, serde_json::Value),
) -> Result<PersistenceEnvelope, PersistenceError> {
    let metadata: EventMetadata = serde_json::from_value(meta_json)
        .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
    Ok(PersistenceEnvelope {
        sequence_nr: seq as u64,
        event_type,
        payload,
        metadata,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "store_projection_test.rs"]
mod projection_tests;

#[cfg(test)]
#[path = "field_index_writes_test.rs"]
mod field_index_writes_tests;

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
        let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
            return;
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
    fn load_query_projection_fields_many_returns_requested_fields() {
        let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
            return;
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
        let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
            return;
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
