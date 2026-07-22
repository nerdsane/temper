//! Source-fenced Turso journal append implementation.

use super::*;

impl TursoEventStore {
    pub(super) async fn append_with_snapshot_source(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        snapshot_source: &SnapshotSourceFence,
    ) -> Result<u64, PersistenceError> {
        if events.is_empty() {
            return Ok(expected_sequence);
        }

        let attempt_timeout = append_attempt_timeout();
        let total_attempts = append_max_attempts();
        let mut last_err = None;
        let bypass_write_gate =
            events.len() == 1 && matches!(snapshot_source, SnapshotSourceFence::Unchecked);
        for attempt in 0..total_attempts {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(retry_delay_ms(attempt - 1))).await;
            }
            let _high_priority_marker =
                bypass_write_gate.then(|| self.mark_high_priority_write("turso.append"));
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
                self.append_inner(persistence_id, expected_sequence, events, snapshot_source),
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
                Ok(sequence) => {
                    if attempt > 0 {
                        record_turso_write_retry("turso.append", attempt as u64, "succeeded");
                    }
                    return Ok(sequence);
                }
                Err(error) => {
                    let transient = matches!(
                        &error,
                        PersistenceError::Storage(message) if is_transient_write_error(message)
                    );
                    if !transient {
                        return Err(error);
                    }
                    last_err = Some(error);
                }
            }
        }
        record_turso_write_retry("turso.append", total_attempts as u64, "exhausted");
        Err(last_err.expect("retry loop captured at least one error"))
    }

    pub(super) async fn append_inner(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        snapshot_source: &SnapshotSourceFence,
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
                "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
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
        let current_sequence = match rows.next().await.map_err(storage_error)? {
            Some(row) => row.get::<i64>(0).map_err(storage_error)? as u64,
            None => 0,
        };
        drop(rows);
        if current_sequence != expected_sequence {
            let _ = tx.rollback().await;
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: current_sequence,
            });
        }

        Self::validate_snapshot_source(&tx, tenant, entity_type, entity_id, snapshot_source)
            .await?;
        let segment_index =
            Self::prepare_open_segment(&tx, tenant, entity_type, entity_id, current_sequence)
                .await?;

        let mut new_sequence = expected_sequence;
        for event in events {
            new_sequence = new_sequence.checked_add(1).ok_or_else(|| {
                PersistenceError::Storage("journal sequence overflow".to_string())
            })?;
            let payload_json = serde_json::to_string(&event.payload)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            let metadata_json = serde_json::to_string(&event.metadata)
                .map_err(|error| PersistenceError::Serialization(error.to_string()))?;
            if let Err(error) = tx
                .execute(
                    "INSERT INTO events \
                     (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        tenant,
                        entity_type,
                        entity_id,
                        new_sequence as i64,
                        segment_index,
                        event.event_type.as_str(),
                        payload_json,
                        metadata_json,
                    ],
                )
                .await
            {
                let message = error.to_string();
                let _ = tx.rollback().await;
                if message.contains("UNIQUE") {
                    return Err(PersistenceError::ConcurrencyViolation {
                        expected: expected_sequence,
                        actual: new_sequence,
                    });
                }
                return Err(PersistenceError::Storage(message));
            }
        }

        tx.execute(
            "UPDATE event_segments \
             SET end_sequence_nr = ?5, event_count = MAX(?5 - start_sequence_nr + 1, 0) \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND segment_index = ?4",
            params![
                tenant,
                entity_type,
                entity_id,
                segment_index,
                new_sequence as i64
            ],
        )
        .await
        .map_err(storage_error)?;

        if append_retires_snapshot(
            expected_sequence,
            events,
            snapshot_source,
            entity_type,
            entity_id,
        ) {
            tx.execute(
                "DELETE FROM snapshots \
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;
        }
        mark_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
        tx.commit().await.map_err(storage_error)?;
        Ok(new_sequence)
    }
}
