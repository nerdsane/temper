//! Postgres event-store append operations.

use super::*;

impl PostgresEventStore {
    /// Append one or more events to the journal.
    ///
    /// Events are inserted with consecutive sequence numbers starting from
    /// `expected_sequence + 1`.  The UNIQUE index on
    /// `(entity_type, entity_id, sequence_nr)` enforces optimistic
    /// concurrency; a duplicate-key violation is surfaced as
    /// [`PersistenceError::ConcurrencyViolation`].
    ///
    /// Returns the new highest sequence number after the append.
    pub(super) async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.append_with_index_rows(persistence_id, expected_sequence, events, &[], &[], false)
            .await
    }

    pub(super) async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconcile_vectors: bool,
    ) -> Result<u64, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;

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

        let current_seq = row.map(|r| r.0 as u64).unwrap_or(0);
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
        for key in key_rows {
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
            new_seq += 1;
            let metadata_json = serde_json::to_value(&event.metadata)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;

            if let Err(e) = crate::dbm::postgres_query!(
                "INSERT INTO events (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(new_seq as i64)
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

        // ADR-0153: co-commit the declared key-index rows in THIS transaction
        // (uniqueness was validated above). Drop the entity's prior row for each
        // key_name (the value may have changed), then claim the new key_hash.
        for key in key_rows {
            crate::dbm::postgres_query!(
                "DELETE FROM entity_key_index \
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
                "INSERT INTO entity_key_index \
                 (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(&key.key_name)
            .bind(&key.key_hash)
            .bind(entity_id)
            .bind(new_seq as i64)
            .execute(&mut *tx)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
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
