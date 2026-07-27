use sqlx::PgConnection;
use temper_runtime::persistence::PersistenceError;

fn postgres_sequence(sequence: u64, operation: &str) -> Result<i64, PersistenceError> {
    i64::try_from(sequence).map_err(|_| {
        PersistenceError::Storage(format!(
            "event sequence exceeds PostgreSQL range during {operation}"
        ))
    })
}

pub(crate) async fn open_segment_for_append(
    conn: &mut PgConnection,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    current_seq: u64,
) -> Result<i64, PersistenceError> {
    let open_segments: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT segment_index, start_sequence_nr FROM event_segments \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sealed_at IS NULL \
         ORDER BY segment_index DESC LIMIT 2",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    if open_segments.len() > 1 {
        return Err(PersistenceError::Storage(format!(
            "multiple open event segments for {tenant}:{entity_type}:{entity_id}"
        )));
    }

    let row: (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(segment_index), -1) FROM ( \
           SELECT segment_index FROM events \
            WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
           UNION ALL \
           SELECT segment_index FROM event_segments \
            WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
         ) AS all_segments",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    let last_segment = row.0;
    let next_sequence = postgres_sequence(
        current_seq.checked_add(1).ok_or_else(|| {
            PersistenceError::Storage("event sequence exhausted while opening segment".to_string())
        })?,
        "segment creation",
    )?;
    if let Some((segment_index, start_sequence)) = open_segments.first().copied() {
        if segment_index != last_segment || start_sequence <= 0 || start_sequence > next_sequence {
            return Err(PersistenceError::Storage(format!(
                "invalid open event segment {segment_index} for {tenant}:{entity_type}:{entity_id}"
            )));
        }
        return Ok(segment_index);
    }

    let segment_index = last_segment
        .checked_add(1)
        .ok_or_else(|| PersistenceError::Storage("event segment index exhausted".to_string()))?;
    let inserted = sqlx::query(
        "INSERT INTO event_segments \
         (tenant, entity_type, entity_id, segment_index, start_sequence_nr) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(segment_index)
    .bind(next_sequence)
    .execute(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    if inserted.rows_affected() != 1 {
        return Err(PersistenceError::Storage(format!(
            "segment creation affected {} rows, expected one",
            inserted.rows_affected()
        )));
    }
    Ok(segment_index)
}

pub(crate) async fn update_segment_after_append(
    conn: &mut PgConnection,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    segment_index: i64,
    new_seq: u64,
) -> Result<(), PersistenceError> {
    let new_sequence = postgres_sequence(new_seq, "segment update")?;
    let updated = sqlx::query(
        "UPDATE event_segments \
         SET end_sequence_nr = $5, event_count = GREATEST($5 - start_sequence_nr + 1, 0) \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND segment_index = $4",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(segment_index)
    .bind(new_sequence)
    .execute(conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    if updated.rows_affected() != 1 {
        return Err(PersistenceError::Storage(format!(
            "segment update affected {} rows, expected one",
            updated.rows_affected()
        )));
    }
    Ok(())
}

pub(crate) async fn rotate_after_snapshot(
    conn: &mut PgConnection,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    sequence_nr: u64,
) -> Result<(), PersistenceError> {
    let stored_sequence = postgres_sequence(sequence_nr, "snapshot rotation")?;
    let (journal_tail,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    if stored_sequence == 0 || stored_sequence != journal_tail {
        return Ok(());
    }

    let segment: Option<(i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT segment_index, MIN(sequence_nr), MAX(sequence_nr), COUNT(*) \
         FROM events \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
           AND segment_index = ( \
             SELECT segment_index FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
               AND sequence_nr = $4 \
           ) \
         GROUP BY segment_index",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(stored_sequence)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    let Some((current_segment, segment_start, segment_end, event_count)) = segment else {
        return Err(PersistenceError::Storage(format!(
            "journal tail {stored_sequence} has no event segment"
        )));
    };

    let sealed = sqlx::query(
        "UPDATE event_segments \
         SET start_sequence_nr = $5, end_sequence_nr = $6, \
             snapshot_sequence = $7, event_count = $8, sealed_at = now() \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND segment_index = $4",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(current_segment)
    .bind(segment_start)
    .bind(segment_end)
    .bind(stored_sequence)
    .bind(event_count)
    .execute(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    if sealed.rows_affected() == 0 {
        sqlx::query(
            "INSERT INTO event_segments \
             (tenant, entity_type, entity_id, segment_index, start_sequence_nr, \
              end_sequence_nr, snapshot_sequence, event_count, sealed_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now())",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(current_segment)
        .bind(segment_start)
        .bind(segment_end)
        .bind(stored_sequence)
        .bind(event_count)
        .execute(&mut *conn)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    }
    let _ = open_segment_for_append(conn, tenant, entity_type, entity_id, sequence_nr).await?;
    Ok(())
}
