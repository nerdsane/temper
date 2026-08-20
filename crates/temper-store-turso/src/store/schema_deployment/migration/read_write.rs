use super::*;

pub(super) async fn load_job(
    tx: &libsql::Transaction,
    tenant: &str,
    job_id: &str,
) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
    let mut rows = tx
        .query(
            "SELECT job_json FROM schema_migration_jobs WHERE tenant = ?1 AND job_id = ?2",
            params![tenant, job_id],
        )
        .await
        .map_err(backend)?;
    match rows.next().await.map_err(backend)? {
        Some(row) => decode(&row.get::<String>(0).map_err(backend)?).map(Some),
        None => Ok(None),
    }
}

pub(super) async fn write_job(
    tx: &libsql::Transaction,
    job: &SchemaMigrationJob,
) -> Result<(), SchemaDeploymentStoreError> {
    tx.execute(
        "UPDATE schema_migration_jobs SET job_json = ?3 WHERE tenant = ?1 AND job_id = ?2",
        params![
            job.command.tenant.as_str(),
            job.command.job_id.as_str(),
            encode(job)?
        ],
    )
    .await
    .map_err(backend)?;
    Ok(())
}

pub(super) async fn load_pointer(
    tx: &libsql::Transaction,
    tenant: &str,
    scope_id: &str,
) -> Result<Option<SchemaActivePointer>, SchemaDeploymentStoreError> {
    let mut rows = tx
        .query(
            "SELECT pointer_json FROM schema_active_pointers
             WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3",
            params![tenant, SCOPE_KIND_TASK, scope_id],
        )
        .await
        .map_err(backend)?;
    match rows.next().await.map_err(backend)? {
        Some(row) => decode(&row.get::<String>(0).map_err(backend)?).map(Some),
        None => Ok(None),
    }
}

pub(super) async fn write_pointer(
    tx: &libsql::Transaction,
    pointer: &SchemaActivePointer,
) -> Result<(), SchemaDeploymentStoreError> {
    tx.execute(
        "INSERT INTO schema_active_pointers (tenant, scope_kind, scope_id, pointer_json)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(tenant, scope_kind, scope_id) DO UPDATE SET pointer_json = excluded.pointer_json",
        params![
            pointer.tenant.as_str(),
            SCOPE_KIND_TASK,
            pointer.scope.id.as_str(),
            encode(pointer)?
        ],
    )
    .await
    .map_err(backend)?;
    Ok(())
}

pub(super) async fn load_batch_receipt(
    tx: &libsql::Transaction,
    tenant: &str,
    job_id: &str,
    receipt_id: &str,
) -> Result<Option<SchemaMigrationBatchReceipt>, SchemaDeploymentStoreError> {
    load_receipt(
        tx,
        "schema_migration_batch_receipts",
        tenant,
        job_id,
        receipt_id,
    )
    .await
}

pub(super) async fn load_validation_receipt(
    tx: &libsql::Transaction,
    tenant: &str,
    job_id: &str,
    receipt_id: &str,
) -> Result<Option<SchemaMigrationValidationReceipt>, SchemaDeploymentStoreError> {
    load_receipt(
        tx,
        "schema_migration_validation_receipts",
        tenant,
        job_id,
        receipt_id,
    )
    .await
}

pub(super) async fn load_receipt<T: serde::de::DeserializeOwned>(
    tx: &libsql::Transaction,
    table: &str,
    tenant: &str,
    job_id: &str,
    receipt_id: &str,
) -> Result<Option<T>, SchemaDeploymentStoreError> {
    let sql = format!(
        "SELECT receipt_json FROM {table} WHERE tenant = ?1 AND job_id = ?2 AND receipt_id = ?3"
    );
    let mut rows = tx
        .query(&sql, params![tenant, job_id, receipt_id])
        .await
        .map_err(backend)?;
    match rows.next().await.map_err(backend)? {
        Some(row) => decode(&row.get::<String>(0).map_err(backend)?).map(Some),
        None => Ok(None),
    }
}

pub(super) async fn load_shadow(
    tx: &libsql::Transaction,
    tenant: &str,
    job_id: &str,
    row: &SchemaMigrationShadowRow,
) -> Result<Option<SchemaMigrationShadowRow>, SchemaDeploymentStoreError> {
    let mut rows = tx
        .query(
            "SELECT row_json FROM schema_migration_shadow
             WHERE tenant = ?1 AND job_id = ?2 AND entity_type = ?3 AND entity_id = ?4",
            params![
                tenant,
                job_id,
                row.entity_type.as_str(),
                row.entity_id.as_str()
            ],
        )
        .await
        .map_err(backend)?;
    match rows.next().await.map_err(backend)? {
        Some(found) => decode(&found.get::<String>(0).map_err(backend)?).map(Some),
        None => Ok(None),
    }
}
