//! Postgres event-store batch reads operations.

use super::*;

impl PostgresEventStore {
    /// Atomically append to multiple entity journals in one PostgreSQL
    /// transaction. Used as the storage foundation for cross-actor Composite
    /// transactions: every stream's optimistic-concurrency check must pass
    /// before any event is inserted.
    pub(super) async fn append_batch(
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

        let mut parsed = Vec::with_capacity(appends.len());
        for append in appends {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
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
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

            let current_seq = row.map(|r| r.0 as u64).unwrap_or(0);
            if current_seq != append.expected_sequence {
                transaction_timer.set_outcome("concurrency_violation");
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: append.expected_sequence,
                    actual: current_seq,
                });
            }
            let segment_index = segments::open_segment_for_append(
                &mut tx,
                tenant,
                entity_type,
                entity_id,
                current_seq,
            )
            .await?;
            parsed.push((
                tenant.to_string(),
                entity_type.to_string(),
                entity_id.to_string(),
                segment_index,
            ));
        }

        let mut results = Vec::with_capacity(appends.len());
        for (append, (tenant, entity_type, entity_id, segment_index)) in
            appends.iter().zip(parsed.iter())
        {
            let mut new_seq = append.expected_sequence;
            for event in &append.events {
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
                .bind(*segment_index)
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
                    *segment_index,
                    new_seq,
                )
                .await?;
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
    pub(super) async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.read_events_with_head(persistence_id, from_sequence)
            .await
            .map(|read| read.events)
    }

    pub(super) async fn read_events_with_head(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<JournalRead, PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;

        type JournalRow = (
            Option<i64>,
            Option<String>,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
            i64,
        );
        let rows: Vec<JournalRow> = crate::dbm::postgres_query_as!(
            "WITH journal_head AS ( \
                 SELECT COALESCE(MAX(sequence_nr), 0)::BIGINT AS sequence_nr \
                 FROM events \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
             ), tail AS ( \
                 SELECT sequence_nr, event_type, payload, metadata \
                 FROM events \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
                   AND sequence_nr > $4 \
             ) \
             SELECT tail.sequence_nr, tail.event_type, tail.payload, tail.metadata, \
                    journal_head.sequence_nr \
             FROM journal_head \
             LEFT JOIN tail ON TRUE \
             ORDER BY tail.sequence_nr ASC",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(from_sequence as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))?;

        let journal_head_sequence_nr = rows
            .first()
            .ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "journal head query returned no row for {persistence_id}"
                ))
            })?
            .4
            .try_into()
            .map_err(|_| {
                PersistenceError::Storage(format!("journal head is negative for {persistence_id}"))
            })?;
        let mut events = Vec::with_capacity(rows.len());
        for (sequence_nr, event_type, payload, metadata, _) in rows {
            match (sequence_nr, event_type, payload, metadata) {
                (Some(sequence_nr), Some(event_type), Some(payload), Some(metadata)) => {
                    let sequence_nr = sequence_nr.try_into().map_err(|_| {
                        PersistenceError::Storage(format!(
                            "journal sequence is negative for {persistence_id}"
                        ))
                    })?;
                    let metadata: EventMetadata = serde_json::from_value(metadata)
                        .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
                    events.push(PersistenceEnvelope {
                        sequence_nr,
                        event_type,
                        payload,
                        metadata,
                    });
                }
                (None, None, None, None) => {}
                _ => {
                    return Err(PersistenceError::Serialization(format!(
                        "journal query returned a partial event row for {persistence_id}"
                    )));
                }
            }
        }

        Ok(JournalRead {
            events,
            journal_head_sequence_nr,
        })
    }
}
