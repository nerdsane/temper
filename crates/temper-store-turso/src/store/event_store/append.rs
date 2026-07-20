//! Single-stream append implementations.

use super::*;

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
    pub(super) async fn append_inner(
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
    conn: &super::super::instrumentation::InstrumentedConnection,
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
