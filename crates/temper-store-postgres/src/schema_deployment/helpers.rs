use super::*;

pub(super) fn backend(error: impl std::fmt::Display) -> SchemaDeploymentStoreError {
    SchemaDeploymentStoreError::BackendUnavailable(error.to_string())
}

pub(super) fn validate_digest(name: &str, value: &str) -> Result<(), SchemaDeploymentStoreError> {
    if !temper_runtime::persistence::schema_deployment::is_canonical_sha256_digest(value) {
        return Err(SchemaDeploymentStoreError::InvalidInput(format!(
            "{name} must use canonical sha256:<64 lowercase hex> form"
        )));
    }
    Ok(())
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, SchemaDeploymentStoreError> {
    serde_json::from_value(value).map_err(backend)
}

pub(super) fn encode<T: serde::Serialize>(
    value: &T,
) -> Result<serde_json::Value, SchemaDeploymentStoreError> {
    serde_json::to_value(value).map_err(backend)
}

pub(super) fn validate_submit(
    command: &SubmitSchemaBundle,
) -> Result<(), SchemaDeploymentStoreError> {
    for (name, value) in [
        ("tenant", command.bundle.tenant.as_str()),
        ("scope id", command.bundle.scope.id.as_str()),
        ("bundle digest", command.bundle.digest.as_str()),
        ("idempotency key", command.idempotency_key.as_str()),
        ("request digest", command.request_digest.as_str()),
        ("request id", command.request_id.as_str()),
    ] {
        let budget = if name.contains("digest") { 128 } else { 256 };
        if value.trim().is_empty() || value.trim() != value || value.len() > budget {
            return Err(SchemaDeploymentStoreError::InvalidInput(format!(
                "{name} must be non-empty, canonical, and at most {budget} bytes"
            )));
        }
    }
    validate_digest("bundle digest", &command.bundle.digest)?;
    validate_digest("request digest", &command.request_digest)?;
    if let Some(predecessor) = command.bundle.predecessor_digest.as_deref() {
        validate_digest("predecessor digest", predecessor)?;
    }
    for digest in command.bundle.wasm_module_digests.values() {
        validate_digest("WASM module digest", digest)?;
    }
    if let Some(digest) = command.bundle.migration_module_digest.as_deref() {
        validate_digest("migration module digest", digest)?;
    }
    Ok(())
}

pub(super) fn validate_operation(
    tenant: &str,
    scope: &SchemaScope,
    digest: &str,
    operation: &SchemaOperationIdentity,
) -> Result<(), SchemaDeploymentStoreError> {
    for (name, value) in [
        ("tenant", tenant),
        ("scope id", scope.id.as_str()),
        ("bundle digest", digest),
        ("idempotency key", operation.idempotency_key.as_str()),
        ("request digest", operation.request_digest.as_str()),
        ("request id", operation.request_id.as_str()),
    ] {
        let budget = if name.contains("digest") { 128 } else { 256 };
        if value.trim().is_empty() || value.trim() != value || value.len() > budget {
            return Err(SchemaDeploymentStoreError::InvalidInput(format!(
                "{name} must be non-empty, canonical, and at most {budget} bytes"
            )));
        }
    }
    validate_digest("bundle digest", digest)?;
    validate_digest("request digest", &operation.request_digest)?;
    Ok(())
}

pub(super) async fn lock_schema_key(
    tx: &mut Transaction<'_, Postgres>,
    namespace: &str,
    parts: &[&str],
) -> Result<(), SchemaDeploymentStoreError> {
    let mut key = String::from("temper-schema:");
    key.push_str(namespace);
    for part in parts {
        key.push('\u{1f}');
        key.push_str(part);
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(key)
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    Ok(())
}

pub(super) async fn locked_idempotency(
    tx: &mut Transaction<'_, Postgres>,
    tenant: &str,
    operation: &str,
    idempotency_key: &str,
) -> Result<Option<(String, String, String)>, SchemaDeploymentStoreError> {
    let row = sqlx::query(
        "SELECT request_digest, bundle_digest, scope_id
         FROM schema_deployment_idempotency
         WHERE tenant = $1 AND operation = $2 AND idempotency_key = $3
         FOR UPDATE",
    )
    .bind(tenant)
    .bind(operation)
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(backend)?;
    Ok(row.map(|row| {
        (
            row.get("request_digest"),
            row.get("bundle_digest"),
            row.get("scope_id"),
        )
    }))
}

pub(super) async fn insert_idempotency(
    tx: &mut Transaction<'_, Postgres>,
    tenant: &str,
    operation_name: &str,
    operation: &SchemaOperationIdentity,
    scope: &SchemaScope,
    bundle_digest: &str,
) -> Result<(), SchemaDeploymentStoreError> {
    sqlx::query(
        "INSERT INTO schema_deployment_idempotency
         (tenant, operation, idempotency_key, request_digest, bundle_digest, scope_kind, scope_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(tenant)
    .bind(operation_name)
    .bind(&operation.idempotency_key)
    .bind(&operation.request_digest)
    .bind(bundle_digest)
    .bind(SCOPE_KIND_TASK)
    .bind(&scope.id)
    .execute(&mut **tx)
    .await
    .map_err(backend)?;
    Ok(())
}

pub(super) async fn locked_deployment(
    tx: &mut Transaction<'_, Postgres>,
    tenant: &str,
    scope: &SchemaScope,
    digest: &str,
) -> Result<Option<SchemaDeploymentRecord>, SchemaDeploymentStoreError> {
    let row = sqlx::query(
        "SELECT record_json FROM schema_deployments
         WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3 AND bundle_digest = $4
         FOR UPDATE",
    )
    .bind(tenant)
    .bind(SCOPE_KIND_TASK)
    .bind(&scope.id)
    .bind(digest)
    .fetch_optional(&mut **tx)
    .await
    .map_err(backend)?;
    row.map(|row| decode(row.get("record_json"))).transpose()
}

pub(super) async fn write_deployment(
    tx: &mut Transaction<'_, Postgres>,
    record: &SchemaDeploymentRecord,
) -> Result<(), SchemaDeploymentStoreError> {
    sqlx::query(
        "UPDATE schema_deployments SET record_json = $5
         WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3 AND bundle_digest = $4",
    )
    .bind(&record.bundle.tenant)
    .bind(SCOPE_KIND_TASK)
    .bind(&record.bundle.scope.id)
    .bind(&record.bundle.digest)
    .bind(encode(record)?)
    .execute(&mut **tx)
    .await
    .map_err(backend)?;
    Ok(())
}
