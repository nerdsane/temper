//! The rows of one append, written inside its transaction.

use temper_runtime::persistence::{
    PersistenceAppend, PersistenceAppendResult, PersistenceError, storage_error,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use super::OwnedAppend;
use crate::driver::{Transaction, Value, params, params_from_iter};
use crate::store::instrumentation::record_turso_query_duration;

const APPEND_BATCH_INSERT_CHUNK_ROWS: usize = 400;

/// Write `append`'s rows in `tx`: its events after a sequence check, its
/// segment's end, the scheduled actions it arms and fires (a row exists
/// exactly while its action waits; a repeated arm keeps its row), and the
/// entity's query-plane, timer and vector rows as its events leave it.
pub(super) async fn write_append(
    tx: &Transaction<'_>,
    append: &OwnedAppend,
) -> Result<u64, PersistenceError> {
    let (tenant, entity_type, entity_id) =
        parse_persistence_id_parts(&append.persistence_id).map_err(PersistenceError::Storage)?;
    let expected_sequence = append.expected_sequence;

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
        tracing::debug!(
            expected = expected_sequence,
            actual = current_seq,
            "concurrency violation on append"
        );
        return Err(PersistenceError::ConcurrencyViolation {
            expected: expected_sequence,
            actual: current_seq,
        });
    }

    let segment_index = open_segment(tx, [tenant, entity_type, entity_id], current_seq).await?;

    let mut new_seq = expected_sequence;
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
            if msg.contains("UNIQUE") {
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: expected_sequence,
                    actual: new_seq,
                });
            }
            tracing::error!(error = %e, "event insert failed");
            return Err(PersistenceError::Storage(msg));
        }
    }

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

    for action in &append.scheduled_actions {
        tx.execute(
            "INSERT INTO scheduled_actions \
                 (tenant, entity_type, entity_id, arm_id, action, due_at, armed_by) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (tenant, entity_type, entity_id, arm_id) DO NOTHING",
            params![
                action.tenant.as_str(),
                action.entity_type.as_str(),
                action.entity_id.as_str(),
                action.arm_id.as_str(),
                action.action.as_str(),
                action.due_at.as_str(),
                action.armed_by.as_str(),
            ],
        )
        .await
        .map_err(storage_error)?;
    }
    if let Some(arm_id) = append.fired_scheduled_action.as_deref() {
        tx.execute(
            "DELETE FROM scheduled_actions \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND arm_id = ?4",
            params![tenant, entity_type, entity_id, arm_id],
        )
        .await
        .map_err(storage_error)?;
    }
    if !append.entity_rows.projects_entity() {
        super::super::projection_delta::mark_pending(
            tx,
            [tenant, entity_type, entity_id],
            new_seq,
            append.events.iter().any(|event| event.is_tombstone()),
        )
        .await?;
    }
    append
        .entity_rows
        .write(tx, [tenant, entity_type, entity_id], new_seq)
        .await?;
    Ok(new_seq)
}

/// The entity's open segment, opened after `current_seq` when it has none.
async fn open_segment(
    tx: &Transaction<'_>,
    [tenant, entity_type, entity_id]: [&str; 3],
    current_seq: u64,
) -> Result<i64, PersistenceError> {
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
        return row.get::<i64>(0).map_err(storage_error);
    }
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
    let index = match max_rows.next().await.map_err(storage_error)? {
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
            index,
            ((current_seq + 1).max(1)) as i64
        ],
    )
    .await
    .map_err(storage_error)?;
    Ok(index)
}

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

/// Write every journal of a batch in `tx`, after a sequence check of each.
pub(super) async fn write_batch(
    tx: &Transaction<'_>,
    appends: &[PersistenceAppend],
) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
    let mut parsed = Vec::with_capacity(appends.len());
    for append in appends {
        let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&append.persistence_id)
            .map_err(PersistenceError::Storage)?;

        if append.expected_sequence != 0 || append.events.is_empty() {
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
                tracing::debug!(
                    expected = append.expected_sequence,
                    actual = current_seq,
                    persistence_id = %append.persistence_id,
                    "concurrency violation on append_batch"
                );
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: append.expected_sequence,
                    actual: current_seq,
                });
            }
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
            if msg.contains("UNIQUE") {
                let first = &chunk[0];
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: first.expected_sequence,
                    actual: first.sequence_nr,
                });
            }
            tracing::error!(error = %e, "event batch insert failed");
            return Err(PersistenceError::Storage(msg));
        }
    }
    // Each record's query-plane row commits with its events, as in a single
    // append; a record the batch does not project is marked pending instead.
    for ((append, key), result) in appends.iter().zip(&parsed).zip(&results) {
        let rows = super::EntityRows::new(&append.commit_rows());
        let record = [key.0.as_str(), key.1.as_str(), key.2.as_str()];
        if !rows.projects_entity() && !append.events.is_empty() {
            super::super::projection_delta::mark_pending(
                tx,
                record,
                result.sequence_nr,
                append.events.iter().any(|event| event.is_tombstone()),
            )
            .await?;
        }
        rows.write(tx, record, result.sequence_nr).await?;
    }
    Ok(results)
}
