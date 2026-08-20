use super::*;

pub(super) async fn locked_job(
    tx: &mut Transaction<'_, Postgres>,
    tenant: &str,
    job_id: &str,
) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
    let row = sqlx::query(
        "SELECT job_json FROM schema_migration_jobs WHERE tenant = $1 AND job_id = $2 FOR UPDATE",
    )
    .bind(tenant)
    .bind(job_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(backend)?;
    row.map(|row| decode(row.get("job_json"))).transpose()
}

pub(super) async fn write_job(
    tx: &mut Transaction<'_, Postgres>,
    job: &SchemaMigrationJob,
) -> Result<(), SchemaDeploymentStoreError> {
    sqlx::query("UPDATE schema_migration_jobs SET job_json = $3 WHERE tenant = $1 AND job_id = $2")
        .bind(&job.command.tenant)
        .bind(&job.command.job_id)
        .bind(encode(job)?)
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    Ok(())
}

pub(super) async fn locked_pointer(
    tx: &mut Transaction<'_, Postgres>,
    tenant: &str,
    scope_id: &str,
) -> Result<Option<SchemaActivePointer>, SchemaDeploymentStoreError> {
    let row = sqlx::query(
        "SELECT pointer_json FROM schema_active_pointers
         WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3 FOR UPDATE",
    )
    .bind(tenant)
    .bind(SCOPE_KIND_TASK)
    .bind(scope_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(backend)?;
    row.map(|row| decode(row.get("pointer_json"))).transpose()
}

pub(super) async fn load_receipt<T: serde::de::DeserializeOwned>(
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
    tenant: &str,
    job_id: &str,
    receipt_id: &str,
) -> Result<Option<T>, SchemaDeploymentStoreError> {
    let sql = format!(
        "SELECT receipt_json FROM {table} WHERE tenant = $1 AND job_id = $2 AND receipt_id = $3 FOR UPDATE"
    );
    let row = sqlx::query(&sql)
        .bind(tenant)
        .bind(job_id)
        .bind(receipt_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(backend)?;
    row.map(|row| decode(row.get("receipt_json"))).transpose()
}
