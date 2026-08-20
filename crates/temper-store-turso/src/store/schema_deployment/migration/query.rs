use super::*;

pub(crate) async fn get(
    store: &TursoEventStore,
    tenant: &str,
    job_id: &str,
) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
    let connection = store.configured_connection().await.map_err(backend)?;
    let mut rows = connection
        .query(
            "SELECT job_json FROM schema_migration_jobs WHERE tenant = ?1 AND job_id = ?2",
            params![tenant, job_id],
        )
        .await
        .map_err(backend)?;
    match rows.next().await.map_err(backend)? {
        Some(row) => {
            decode::<SchemaMigrationJob>(&row.get::<String>(0).map_err(backend)?).map(Some)
        }
        None => Ok(None),
    }
}

pub(crate) async fn get_in_scope(
    store: &TursoEventStore,
    tenant: &str,
    scope: &SchemaScope,
    job_id: &str,
) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
    let connection = store.configured_connection().await.map_err(backend)?;
    let mut rows = connection
        .query(
            "SELECT job_json FROM schema_migration_jobs
             WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3 AND job_id = ?4",
            params![tenant, SCOPE_KIND_TASK, scope.id.as_str(), job_id],
        )
        .await
        .map_err(backend)?;
    match rows.next().await.map_err(backend)? {
        Some(row) => {
            decode::<SchemaMigrationJob>(&row.get::<String>(0).map_err(backend)?).map(Some)
        }
        None => Ok(None),
    }
}

pub(crate) async fn list_incomplete(
    store: &TursoEventStore,
    limit: usize,
) -> Result<Vec<SchemaMigrationJob>, SchemaDeploymentStoreError> {
    if limit == 0 {
        return Err(SchemaDeploymentStoreError::InvalidInput(
            "migration list budget must be positive".into(),
        ));
    }
    let limit = i64::try_from(limit).map_err(|_| {
        SchemaDeploymentStoreError::InvalidInput("migration list budget is too large".into())
    })?;
    let connection = store.configured_connection().await.map_err(backend)?;
    let mut rows = connection
        .query(
            "SELECT job_json FROM schema_migration_jobs
             WHERE json_extract(job_json, '$.status') NOT IN ('Completed', 'Rejected')
             ORDER BY tenant, job_id LIMIT ?1",
            params![limit],
        )
        .await
        .map_err(backend)?;
    let mut jobs = Vec::new();
    while let Some(row) = rows.next().await.map_err(backend)? {
        jobs.push(decode::<SchemaMigrationJob>(
            &row.get::<String>(0).map_err(backend)?,
        )?);
    }
    Ok(jobs)
}

pub(crate) async fn claim(
    store: &TursoEventStore,
    tenant: &str,
    job_id: &str,
    logical_now: u64,
    lease_expires_at: u64,
) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
    if lease_expires_at <= logical_now {
        return Err(SchemaDeploymentStoreError::InvalidInput(
            "migration lease must end after logical now".into(),
        ));
    }
    let _permit = store
        .acquire_write_permit("schema_migration_claim", WritePriority::High)
        .await
        .map_err(backend)?;
    let connection = store.configured_connection().await.map_err(backend)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(backend)?;
    let mut job = load_job(&tx, tenant, job_id)
        .await?
        .ok_or(SchemaDeploymentStoreError::NotFound)?;
    let claimable = job.status == SchemaMigrationStatus::Submitted
        || (job.status == SchemaMigrationStatus::Migrating
            && job
                .lease_expires_at
                .is_some_and(|deadline| deadline <= logical_now));
    if !claimable {
        return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
    }
    if job.consumed_attempts >= job.command.budgets.attempts {
        return Err(SchemaDeploymentStoreError::MigrationBudgetExhausted);
    }
    job.status = SchemaMigrationStatus::Migrating;
    job.fence = checked_add(job.fence, "migration fence")?;
    job.committed_sequence = checked_add(job.committed_sequence, "migration sequence")?;
    job.consumed_attempts = job
        .consumed_attempts
        .checked_add(1)
        .ok_or(SchemaDeploymentStoreError::MigrationBudgetExhausted)?;
    job.lease_expires_at = Some(lease_expires_at);
    write_job(&tx, &job).await?;
    tx.commit().await.map_err(backend)?;
    Ok(job)
}
