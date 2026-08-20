use super::*;

pub(crate) async fn page_shadow(
    store: &PostgresEventStore,
    tenant: &str,
    job_id: &str,
    after: Option<(&str, &str)>,
    limit: usize,
) -> Result<Vec<SchemaMigrationShadowRow>, SchemaDeploymentStoreError> {
    if limit == 0 {
        return Err(SchemaDeploymentStoreError::InvalidInput(
            "shadow page budget must be positive".into(),
        ));
    }
    let (after_type, after_id) = after.unwrap_or(("", ""));
    let rows = sqlx::query(
        "SELECT row_json FROM schema_migration_shadow
         WHERE tenant = $1 AND job_id = $2
           AND (entity_type > $3 OR (entity_type = $3 AND entity_id > $4))
         ORDER BY entity_type, entity_id LIMIT $5",
    )
    .bind(tenant)
    .bind(job_id)
    .bind(after_type)
    .bind(after_id)
    .bind(i64::try_from(limit).map_err(backend)?)
    .fetch_all(store.pool())
    .await
    .map_err(backend)?;
    rows.into_iter()
        .map(|row| decode(row.get("row_json")))
        .collect()
}

pub(crate) async fn complete(
    store: &PostgresEventStore,
    tenant: &str,
    job_id: &str,
    expected_fence: u64,
) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
    let mut connection = store.pool().acquire().await.map_err(backend)?;
    let mut tx = connection.begin().await.map_err(backend)?;
    let mut job = locked_job(&mut tx, tenant, job_id)
        .await?
        .ok_or(SchemaDeploymentStoreError::NotFound)?;
    if job.status == SchemaMigrationStatus::Completed {
        tx.commit().await.map_err(backend)?;
        return Ok(job);
    }
    if job.status != SchemaMigrationStatus::CutOver {
        return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
    }
    if job.fence != expected_fence {
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    job.status = SchemaMigrationStatus::Completed;
    job.committed_sequence = checked_add(job.committed_sequence, "migration sequence")?;
    write_job(&mut tx, &job).await?;
    tx.commit().await.map_err(backend)?;
    Ok(job)
}
