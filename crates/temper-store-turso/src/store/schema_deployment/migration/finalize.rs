use super::*;

pub(crate) async fn page_shadow(
    store: &TursoEventStore,
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
    let connection = store.configured_connection().await.map_err(backend)?;
    let mut rows = connection
        .query(
            "SELECT row_json FROM schema_migration_shadow
             WHERE tenant = ?1 AND job_id = ?2
               AND (entity_type > ?3 OR (entity_type = ?3 AND entity_id > ?4))
             ORDER BY entity_type, entity_id LIMIT ?5",
            params![tenant, job_id, after_type, after_id, limit as i64],
        )
        .await
        .map_err(backend)?;
    let mut result = Vec::new();
    while let Some(row) = rows.next().await.map_err(backend)? {
        result.push(decode(&row.get::<String>(0).map_err(backend)?)?);
    }
    Ok(result)
}

pub(crate) async fn complete(
    store: &TursoEventStore,
    tenant: &str,
    job_id: &str,
    expected_fence: u64,
) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
    let _permit = store
        .acquire_write_permit("schema_migration_complete", WritePriority::High)
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
    write_job(&tx, &job).await?;
    tx.commit().await.map_err(backend)?;
    Ok(job)
}
