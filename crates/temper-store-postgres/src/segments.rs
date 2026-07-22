use sqlx::PgConnection;
use temper_runtime::persistence::PersistenceError;

async fn rebuild_open_segment_from_journal(
    conn: &mut PgConnection,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    current_seq: u64,
) -> Result<i64, PersistenceError> {
    sqlx::query(
        "UPDATE events SET segment_index = 0 \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .execute(&mut *conn)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
    sqlx::query(
        "DELETE FROM event_segments \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .execute(&mut *conn)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
    sqlx::query(
        "INSERT INTO event_segments \
         (tenant, entity_type, entity_id, segment_index, start_sequence_nr, end_sequence_nr, event_count) \
         VALUES ($1, $2, $3, 0, 1, $4, $4)",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(i64::try_from(current_seq).map_err(|_| {
        PersistenceError::Storage("journal sequence exceeds PostgreSQL bigint".to_string())
    })?)
    .execute(conn)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
    Ok(0)
}

pub(crate) async fn open_segment_for_append(
    conn: &mut PgConnection,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    current_seq: u64,
) -> Result<i64, PersistenceError> {
    if current_seq == 0 {
        // Snapshot-only generations do not own journal segments. Remove legacy
        // metadata before assigning the first real event to segment zero.
        sqlx::query(
            "DELETE FROM event_segments \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .execute(&mut *conn)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        sqlx::query(
            "INSERT INTO event_segments \
             (tenant, entity_type, entity_id, segment_index, start_sequence_nr) \
             VALUES ($1, $2, $3, 0, 1)",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .execute(conn)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        return Ok(0);
    }
    let segment_row: Option<(i64, i64, Option<i64>)> = sqlx::query_as(
        "SELECT segment_index, start_sequence_nr, end_sequence_nr FROM event_segments \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sealed_at IS NULL \
         ORDER BY segment_index DESC LIMIT 1",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;

    match segment_row {
        Some((idx, start, end))
            if start <= current_seq.saturating_add(1) as i64
                && end.is_none_or(|end| end >= start) =>
        {
            Ok(idx)
        }
        Some(_) => {
            rebuild_open_segment_from_journal(conn, tenant, entity_type, entity_id, current_seq)
                .await
        }
        None => {
            let segment_count: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM event_segments \
                 WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(|error| PersistenceError::Storage(error.to_string()))?;
            if segment_count.0 == 0 {
                return rebuild_open_segment_from_journal(
                    conn,
                    tenant,
                    entity_type,
                    entity_id,
                    current_seq,
                )
                .await;
            }
            let row: (i64,) = sqlx::query_as(
                "SELECT GREATEST(\
                    COALESCE((SELECT MAX(segment_index) FROM events \
                      WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3), -1), \
                    COALESCE((SELECT MAX(segment_index) FROM event_segments \
                      WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3), -1)\
                 ) + 1",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            sqlx::query(
                "INSERT INTO event_segments \
                 (tenant, entity_type, entity_id, segment_index, start_sequence_nr) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (tenant, entity_type, entity_id, segment_index) DO NOTHING",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(row.0)
            .bind((current_seq + 1).max(1) as i64)
            .execute(&mut *conn)
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;
            Ok(row.0)
        }
    }
}

pub(crate) async fn update_segment_after_append(
    conn: &mut PgConnection,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    segment_index: i64,
    new_seq: u64,
) -> Result<(), PersistenceError> {
    sqlx::query(
        "UPDATE event_segments \
         SET end_sequence_nr = $5, event_count = GREATEST($5 - start_sequence_nr + 1, 0) \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND segment_index = $4",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(segment_index)
    .bind(new_seq as i64)
    .execute(conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    Ok(())
}

pub(crate) async fn rotate_after_snapshot(
    conn: &mut PgConnection,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    sequence_nr: u64,
) -> Result<(), PersistenceError> {
    let boundary = i64::try_from(sequence_nr).map_err(|_| {
        PersistenceError::Storage("snapshot sequence exceeds PostgreSQL bigint".to_string())
    })?;
    let journal: (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
    if journal.0 == 0 {
        sqlx::query(
            "DELETE FROM event_segments \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .execute(conn)
        .await
        .map_err(|error| PersistenceError::Storage(error.to_string()))?;
        return Ok(());
    }
    if boundary == 0 || boundary > journal.0 {
        // A snapshot is a recovery accelerator, not a journal boundary. A
        // migration snapshot ahead of the durable journal must not manufacture
        // sealed ranges or an open segment beyond the journal high-water.
        return Ok(());
    }
    let row: (i64,) = sqlx::query_as(
        "SELECT COALESCE(\
             (SELECT MAX(segment_index) FROM events \
              WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr <= $4), \
             (SELECT MAX(segment_index) FROM event_segments \
              WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3), \
             0)",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(boundary)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    let current_segment = row.0;
    let current_start: (i64,) = sqlx::query_as(
        "SELECT COALESCE(\
             (SELECT start_sequence_nr FROM event_segments \
              WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND segment_index = $4), \
             (SELECT MIN(sequence_nr) FROM events \
              WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND segment_index = $4), \
             1)",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(current_segment)
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
    let current_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM events \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
           AND segment_index = $4 AND sequence_nr <= $5",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(current_segment)
    .bind(boundary)
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;

    sqlx::query(
        "INSERT INTO event_segments \
         (tenant, entity_type, entity_id, segment_index, start_sequence_nr, end_sequence_nr, snapshot_sequence, event_count, sealed_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $6, $7, now()) \
         ON CONFLICT (tenant, entity_type, entity_id, segment_index) DO NOTHING",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(current_segment)
    .bind(current_start.0)
    .bind(boundary)
    .bind(current_count.0)
    .execute(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;

    sqlx::query(
        "UPDATE event_segments \
         SET end_sequence_nr = $5, snapshot_sequence = $5, sealed_at = now(), \
             event_count = $6 \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND segment_index = $4",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(current_segment)
    .bind(boundary)
    .bind(current_count.0)
    .execute(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;

    let next_segment = current_segment
        .checked_add(1)
        .ok_or_else(|| PersistenceError::Storage("event segment index overflow".to_string()))?;
    sqlx::query(
        "UPDATE events SET segment_index = $5 \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
           AND segment_index = $4 AND sequence_nr > $6",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(current_segment)
    .bind(next_segment)
    .bind(boundary)
    .execute(&mut *conn)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;

    let tail: (i64, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*), MAX(sequence_nr) FROM events \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 \
           AND segment_index = $4 AND sequence_nr > $5",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(next_segment)
    .bind(boundary)
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| PersistenceError::Storage(error.to_string()))?;
    let next_start = boundary.checked_add(1).ok_or_else(|| {
        PersistenceError::Storage("snapshot successor sequence overflow".to_string())
    })?;
    sqlx::query(
        "INSERT INTO event_segments \
         (tenant, entity_type, entity_id, segment_index, start_sequence_nr, end_sequence_nr, snapshot_sequence, event_count, sealed_at) \
         VALUES ($1, $2, $3, $4, $5, $6, NULL, $7, NULL) \
         ON CONFLICT (tenant, entity_type, entity_id, segment_index) DO UPDATE SET \
           start_sequence_nr = EXCLUDED.start_sequence_nr, \
           end_sequence_nr = EXCLUDED.end_sequence_nr, \
           snapshot_sequence = NULL, \
           event_count = EXCLUDED.event_count, \
           sealed_at = NULL",
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(next_segment)
    .bind(next_start)
    .bind(tail.1)
    .bind(tail.0)
    .execute(conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;

    Ok(())
}
