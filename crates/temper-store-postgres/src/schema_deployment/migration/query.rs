use super::*;

pub(crate) async fn get(
    store: &PostgresEventStore,
    tenant: &str,
    job_id: &str,
) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
    let row =
        sqlx::query("SELECT job_json FROM schema_migration_jobs WHERE tenant = $1 AND job_id = $2")
            .bind(tenant)
            .bind(job_id)
            .fetch_optional(store.pool())
            .await
            .map_err(backend)?;
    row.map(|row| decode(row.get("job_json"))).transpose()
}

pub(crate) async fn get_in_scope(
    store: &PostgresEventStore,
    tenant: &str,
    scope: &SchemaScope,
    job_id: &str,
) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
    let row = sqlx::query(
        "SELECT job_json FROM schema_migration_jobs
         WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3 AND job_id = $4",
    )
    .bind(tenant)
    .bind(SCOPE_KIND_TASK)
    .bind(&scope.id)
    .bind(job_id)
    .fetch_optional(store.pool())
    .await
    .map_err(backend)?;
    row.map(|row| decode(row.get("job_json"))).transpose()
}

pub(crate) async fn list_incomplete(
    store: &PostgresEventStore,
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
    let rows = sqlx::query(
        "SELECT job_json FROM schema_migration_jobs
         WHERE job_json->>'status' NOT IN ('Completed', 'Rejected')
         ORDER BY tenant, job_id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(store.pool())
    .await
    .map_err(backend)?;
    rows.into_iter()
        .map(|row| decode(row.get("job_json")))
        .collect()
}

pub(crate) async fn claim(
    store: &PostgresEventStore,
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
    let mut connection = store.pool().acquire().await.map_err(backend)?;
    let mut tx = connection.begin().await.map_err(backend)?;
    let mut job = locked_job(&mut tx, tenant, job_id)
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
    write_job(&mut tx, &job).await?;
    tx.commit().await.map_err(backend)?;
    Ok(job)
}
