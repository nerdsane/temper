use sqlx::PgConnection;
use temper_runtime::persistence::PersistenceError;

pub(crate) async fn rotate_after_snapshot(
    schema: &crate::PostgresSchema,
    conn: &mut PgConnection,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    sequence_nr: u64,
) -> Result<(), PersistenceError> {
    let row: (i64,) = sqlx::query_as(&schema.qualify_sql(
        "SELECT COALESCE(MAX(segment_index), 0) FROM {schema}events \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr <= $4",
    ))
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(sequence_nr as i64)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;
    let current_segment = row.0;

    sqlx::query(
        &schema.qualify_sql("INSERT INTO {schema}event_segments \
         (tenant, entity_type, entity_id, segment_index, start_sequence_nr, end_sequence_nr, snapshot_sequence, event_count, sealed_at) \
         VALUES ($1, $2, $3, $4, 1, $5, $5, $5, now()) \
         ON CONFLICT (tenant, entity_type, entity_id, segment_index) DO NOTHING"),
    )
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(current_segment)
    .bind(sequence_nr as i64)
    .execute(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;

    sqlx::query(&schema.qualify_sql(
        "UPDATE {schema}event_segments \
         SET end_sequence_nr = $5, snapshot_sequence = $5, sealed_at = now(), \
             event_count = GREATEST($5 - start_sequence_nr + 1, 0) \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND segment_index = $4",
    ))
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(current_segment)
    .bind(sequence_nr as i64)
    .execute(&mut *conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;

    sqlx::query(&schema.qualify_sql(
        "INSERT INTO {schema}event_segments \
         (tenant, entity_type, entity_id, segment_index, start_sequence_nr) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (tenant, entity_type, entity_id, segment_index) DO NOTHING",
    ))
    .bind(tenant)
    .bind(entity_type)
    .bind(entity_id)
    .bind(current_segment + 1)
    .bind(sequence_nr as i64 + 1)
    .execute(conn)
    .await
    .map_err(|e| PersistenceError::Storage(e.to_string()))?;

    Ok(())
}
