//! Atomic multi-stream append implementation.

use super::*;

impl TursoEventStore {
    pub(super) async fn append_batch_inner(
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

        tx.commit().await.map_err(storage_error)?;
        Ok(results)
    }
}
