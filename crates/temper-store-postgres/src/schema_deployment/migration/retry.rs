use super::*;

pub(crate) async fn reserve_retry(
    store: &PostgresEventStore,
    command: ReserveSchemaMigrationRetry,
) -> Result<SchemaMigrationRetryReservation, SchemaDeploymentStoreError> {
    validate_text("tenant", &command.tenant)?;
    validate_text("job_id", &command.job_id)?;
    validate_text("idempotency_key", &command.operation.idempotency_key)?;
    validate_text("request_digest", &command.operation.request_digest)?;
    validate_text("request_id", &command.operation.request_id)?;
    let mut connection = store.pool().acquire().await.map_err(backend)?;
    let mut tx = connection.begin().await.map_err(backend)?;
    lock_schema_key(
        &mut tx,
        "idempotency",
        &[
            &command.tenant,
            "migration-retry",
            &command.operation.idempotency_key,
        ],
    )
    .await?;
    let job = locked_job(&mut tx, &command.tenant, &command.job_id)
        .await?
        .ok_or(SchemaDeploymentStoreError::NotFound)?;
    let prior = sqlx::query(
        "SELECT request_digest, job_id, starting_sequence, request_id
         FROM schema_migration_retry_idempotency
         WHERE tenant = $1 AND idempotency_key = $2 FOR UPDATE",
    )
    .bind(&command.tenant)
    .bind(&command.operation.idempotency_key)
    .fetch_optional(&mut *tx)
    .await
    .map_err(backend)?;
    if let Some(row) = prior {
        if row.get::<String, _>("request_digest") != command.operation.request_digest
            || row.get::<String, _>("job_id") != command.job_id
        {
            return Err(SchemaDeploymentStoreError::IdempotencyConflict);
        }
        let starting_sequence = u64::try_from(row.get::<i64, _>("starting_sequence"))
            .map_err(|_| backend("invalid migration retry starting sequence"))?;
        tx.commit().await.map_err(backend)?;
        return Ok(SchemaMigrationRetryReservation {
            job,
            starting_sequence,
            replayed: true,
            accepted_request_id: row.get("request_id"),
        });
    }
    let starting_sequence = job.committed_sequence;
    sqlx::query("INSERT INTO schema_migration_retry_idempotency (tenant, idempotency_key, request_digest, job_id, starting_sequence, request_id) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(&command.tenant)
        .bind(&command.operation.idempotency_key)
        .bind(&command.operation.request_digest)
        .bind(&command.job_id)
        .bind(i64::try_from(starting_sequence).map_err(backend)?)
        .bind(&command.operation.request_id)
        .execute(&mut *tx).await.map_err(backend)?;
    tx.commit().await.map_err(backend)?;
    Ok(SchemaMigrationRetryReservation {
        job,
        starting_sequence,
        replayed: false,
        accepted_request_id: command.operation.request_id,
    })
}
