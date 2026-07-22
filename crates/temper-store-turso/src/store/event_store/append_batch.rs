//! Atomic source-fenced multi-stream Turso appends.

use super::*;

impl TursoEventStore {
    pub(super) async fn append_batch_inner(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        let mut seen = std::collections::BTreeSet::new();
        let mut batch_claim = None;
        for append in appends {
            if !seen.insert(append.persistence_id.as_str()) {
                return Err(PersistenceError::Storage(format!(
                    "duplicate persistence_id '{}' in append_batch",
                    append.persistence_id
                )));
            }
            if let Some(claim) = &append.batch_idempotency
                && batch_claim.replace(claim).is_some()
            {
                return Err(PersistenceError::Storage(
                    "append_batch supplied more than one idempotency claim".to_string(),
                ));
            }
        }

        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        if let Some(claim) = batch_claim {
            let mut rows = tx
                .query(
                    "SELECT intent_hash FROM persistence_batch_idempotency \
                     WHERE persistence_id = ?1 AND idempotency_key = ?2",
                    params![
                        claim.persistence_id.as_str(),
                        claim.idempotency_key.as_str()
                    ],
                )
                .await
                .map_err(storage_error)?;
            if let Some(row) = rows.next().await.map_err(storage_error)? {
                let committed_hash = row.get::<String>(0).map_err(storage_error)?;
                if committed_hash != claim.intent_hash {
                    return Err(PersistenceError::Storage(format!(
                        "atomic batch idempotency key '{}' was reused with a different intent",
                        claim.idempotency_key
                    )));
                }
                drop(rows);
                let mut results = Vec::with_capacity(appends.len());
                for append in appends {
                    let (tenant, entity_type, entity_id) =
                        parse_persistence_id_parts(&append.persistence_id)
                            .map_err(PersistenceError::Storage)?;
                    let mut sequence_rows = tx
                        .query(
                            "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
                             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                            params![tenant, entity_type, entity_id],
                        )
                        .await
                        .map_err(storage_error)?;
                    let sequence_nr = match sequence_rows.next().await.map_err(storage_error)? {
                        Some(row) => row.get::<i64>(0).map_err(storage_error)? as u64,
                        None => 0,
                    };
                    results.push(PersistenceAppendResult {
                        persistence_id: append.persistence_id.clone(),
                        sequence_nr,
                        batch_already_applied: true,
                    });
                }
                return Ok(results);
            }
        }

        let mut parsed = Vec::with_capacity(appends.len());
        for append in appends {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            let mut rows = tx
                .query(
                    "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                    params![tenant, entity_type, entity_id],
                )
                .await
                .map_err(storage_error)?;
            let current_sequence = match rows.next().await.map_err(storage_error)? {
                Some(row) => row.get::<i64>(0).map_err(storage_error)? as u64,
                None => 0,
            };
            drop(rows);
            if current_sequence != append.expected_sequence {
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: append.expected_sequence,
                    actual: current_sequence,
                });
            }
            Self::validate_snapshot_source(
                &tx,
                tenant,
                entity_type,
                entity_id,
                &append.snapshot_source,
            )
            .await?;
            let segment_index =
                Self::prepare_open_segment(&tx, tenant, entity_type, entity_id, current_sequence)
                    .await?;
            parsed.push((
                tenant.to_string(),
                entity_type.to_string(),
                entity_id.to_string(),
                segment_index,
            ));
        }

        let mut results = Vec::with_capacity(appends.len());
        let mut event_rows = Vec::new();
        for (append, (tenant, entity_type, entity_id, segment_index)) in
            appends.iter().zip(parsed.iter())
        {
            let mut new_sequence = append.expected_sequence;
            for event in &append.events {
                new_sequence = new_sequence.checked_add(1).ok_or_else(|| {
                    PersistenceError::Storage(format!(
                        "append_batch sequence overflow for '{}'",
                        append.persistence_id
                    ))
                })?;
                event_rows.push(PreparedEventInsert {
                    tenant: tenant.clone(),
                    entity_type: entity_type.clone(),
                    entity_id: entity_id.clone(),
                    sequence_nr: new_sequence,
                    segment_index: *segment_index,
                    event_type: event.event_type.clone(),
                    payload_json: serde_json::to_string(&event.payload)
                        .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
                    metadata_json: serde_json::to_string(&event.metadata)
                        .map_err(|error| PersistenceError::Serialization(error.to_string()))?,
                    expected_sequence: append.expected_sequence,
                });
            }
            results.push(PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr: new_sequence,
                batch_already_applied: false,
            });
        }

        for chunk in event_rows.chunks(APPEND_BATCH_INSERT_CHUNK_ROWS) {
            let mut sql = String::from(
                "INSERT INTO events \
                 (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata) VALUES ",
            );
            let mut values = Vec::with_capacity(chunk.len() * 8);
            for (index, row) in chunk.iter().enumerate() {
                if index > 0 {
                    sql.push_str(", ");
                }
                sql.push_str("(?, ?, ?, ?, ?, ?, ?, ?)");
                values.extend([
                    Value::from(row.tenant.clone()),
                    Value::from(row.entity_type.clone()),
                    Value::from(row.entity_id.clone()),
                    Value::from(row.sequence_nr as i64),
                    Value::from(row.segment_index),
                    Value::from(row.event_type.clone()),
                    Value::from(row.payload_json.clone()),
                    Value::from(row.metadata_json.clone()),
                ]);
            }
            if let Err(error) = tx.execute(&sql, params_from_iter(values)).await {
                let message = error.to_string();
                if message.contains("UNIQUE") {
                    let first = &chunk[0];
                    return Err(PersistenceError::ConcurrencyViolation {
                        expected: first.expected_sequence,
                        actual: first.sequence_nr,
                    });
                }
                return Err(PersistenceError::Storage(message));
            }
        }

        for (append, (tenant, entity_type, entity_id, segment_index)) in
            appends.iter().zip(parsed.iter())
        {
            let result = results
                .iter()
                .find(|result| result.persistence_id == append.persistence_id)
                .expect("one append result per parsed stream");
            if result.sequence_nr > append.expected_sequence {
                tx.execute(
                    "UPDATE event_segments \
                     SET end_sequence_nr = ?5, event_count = MAX(?5 - start_sequence_nr + 1, 0) \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND segment_index = ?4",
                    params![
                        tenant.as_str(),
                        entity_type.as_str(),
                        entity_id.as_str(),
                        *segment_index,
                        result.sequence_nr as i64
                    ],
                )
                .await
                .map_err(storage_error)?;
                mark_query_projection_dirty(&tx, tenant, entity_type, entity_id).await?;
            }
            if append_retires_snapshot(
                append.expected_sequence,
                &append.events,
                &append.snapshot_source,
                entity_type,
                entity_id,
            ) {
                tx.execute(
                    "DELETE FROM snapshots \
                     WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                    params![tenant.as_str(), entity_type.as_str(), entity_id.as_str()],
                )
                .await
                .map_err(storage_error)?;
            }
        }

        if let Some(claim) = batch_claim {
            tx.execute(
                "INSERT INTO persistence_batch_idempotency \
                 (persistence_id, idempotency_key, intent_hash) VALUES (?1, ?2, ?3)",
                params![
                    claim.persistence_id.as_str(),
                    claim.idempotency_key.as_str(),
                    claim.intent_hash.as_str(),
                ],
            )
            .await
            .map_err(storage_error)?;
        }

        tx.commit().await.map_err(storage_error)?;
        Ok(results)
    }
}
