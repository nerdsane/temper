use super::*;

pub(crate) async fn reserve_retry(
    store: &TursoEventStore,
    command: ReserveSchemaMigrationRetry,
) -> Result<SchemaMigrationRetryReservation, SchemaDeploymentStoreError> {
    validate_text("tenant", &command.tenant)?;
    validate_text("job_id", &command.job_id)?;
    validate_text("idempotency_key", &command.operation.idempotency_key)?;
    validate_text("request_digest", &command.operation.request_digest)?;
    validate_text("request_id", &command.operation.request_id)?;
    let _permit = store
        .acquire_write_permit("schema_migration_retry_reserve", WritePriority::High)
        .await
        .map_err(backend)?;
    let connection = store.configured_connection().await.map_err(backend)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(backend)?;
    let job = load_job(&tx, &command.tenant, &command.job_id)
        .await?
        .ok_or(SchemaDeploymentStoreError::NotFound)?;
    let mut rows = tx.query("SELECT request_digest, job_id, starting_sequence, request_id FROM schema_migration_retry_idempotency WHERE tenant = ?1 AND idempotency_key = ?2", params![command.tenant.as_str(), command.operation.idempotency_key.as_str()]).await.map_err(backend)?;
    if let Some(row) = rows.next().await.map_err(backend)? {
        let request_digest: String = row.get(0).map_err(backend)?;
        let job_id: String = row.get(1).map_err(backend)?;
        if request_digest != command.operation.request_digest || job_id != command.job_id {
            return Err(SchemaDeploymentStoreError::IdempotencyConflict);
        }
        let starting_sequence =
            u64::try_from(row.get::<i64>(2).map_err(backend)?).map_err(backend)?;
        drop(rows);
        tx.commit().await.map_err(backend)?;
        return Ok(SchemaMigrationRetryReservation {
            job,
            starting_sequence,
            replayed: true,
            accepted_request_id: row.get(3).map_err(backend)?,
        });
    }
    drop(rows);
    let starting_sequence = job.committed_sequence;
    tx.execute("INSERT INTO schema_migration_retry_idempotency (tenant, idempotency_key, request_digest, job_id, starting_sequence, request_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![command.tenant.as_str(), command.operation.idempotency_key.as_str(), command.operation.request_digest.as_str(), command.job_id.as_str(), i64::try_from(starting_sequence).map_err(backend)?, command.operation.request_id.as_str()]).await.map_err(backend)?;
    tx.commit().await.map_err(backend)?;
    Ok(SchemaMigrationRetryReservation {
        job,
        starting_sequence,
        replayed: false,
        accepted_request_id: command.operation.request_id,
    })
}
