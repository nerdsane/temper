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
    value: &str,
) -> Result<T, SchemaDeploymentStoreError> {
    serde_json::from_str(value).map_err(backend)
}

pub(super) fn encode<T: serde::Serialize>(value: &T) -> Result<String, SchemaDeploymentStoreError> {
    serde_json::to_string(value).map_err(backend)
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

pub(super) async fn load_idempotency(
    tx: &libsql::Transaction,
    tenant: &str,
    operation: &str,
    idempotency_key: &str,
) -> Result<Option<(String, String, String)>, SchemaDeploymentStoreError> {
    let mut rows = tx
        .query(
            "SELECT request_digest, bundle_digest, scope_id
             FROM schema_deployment_idempotency
             WHERE tenant = ?1 AND operation = ?2 AND idempotency_key = ?3",
            params![tenant, operation, idempotency_key],
        )
        .await
        .map_err(backend)?;
    let Some(row) = rows.next().await.map_err(backend)? else {
        return Ok(None);
    };
    Ok(Some((
        row.get(0).map_err(backend)?,
        row.get(1).map_err(backend)?,
        row.get(2).map_err(backend)?,
    )))
}

pub(super) async fn insert_idempotency(
    tx: &libsql::Transaction,
    tenant: &str,
    operation_name: &str,
    operation: &SchemaOperationIdentity,
    scope: &SchemaScope,
    bundle_digest: &str,
) -> Result<(), SchemaDeploymentStoreError> {
    tx.execute(
        "INSERT INTO schema_deployment_idempotency
         (tenant, operation, idempotency_key, request_digest, bundle_digest, scope_kind, scope_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            tenant,
            operation_name,
            operation.idempotency_key.as_str(),
            operation.request_digest.as_str(),
            bundle_digest,
            SCOPE_KIND_TASK,
            scope.id.as_str()
        ],
    )
    .await
    .map_err(backend)?;
    Ok(())
}

pub(super) async fn load_deployment(
    tx: &libsql::Transaction,
    tenant: &str,
    scope: &SchemaScope,
    digest: &str,
) -> Result<Option<SchemaDeploymentRecord>, SchemaDeploymentStoreError> {
    let mut rows = tx
        .query(
            "SELECT record_json FROM schema_deployments
             WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3 AND bundle_digest = ?4",
            params![tenant, SCOPE_KIND_TASK, scope.id.as_str(), digest],
        )
        .await
        .map_err(backend)?;
    let Some(row) = rows.next().await.map_err(backend)? else {
        return Ok(None);
    };
    let json: String = row.get(0).map_err(backend)?;
    decode(&json).map(Some)
}

pub(super) async fn write_deployment(
    tx: &libsql::Transaction,
    record: &SchemaDeploymentRecord,
) -> Result<(), SchemaDeploymentStoreError> {
    tx.execute(
        "UPDATE schema_deployments SET record_json = ?5
         WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3 AND bundle_digest = ?4",
        params![
            record.bundle.tenant.as_str(),
            SCOPE_KIND_TASK,
            record.bundle.scope.id.as_str(),
            record.bundle.digest.as_str(),
            encode(record)?
        ],
    )
    .await
    .map_err(backend)?;
    Ok(())
}

pub(super) async fn load_active_pointer(
    store: &TursoEventStore,
    tenant: &str,
    scope: &SchemaScope,
) -> Result<Option<SchemaActivePointer>, SchemaDeploymentStoreError> {
    let connection = store.configured_connection().await.map_err(backend)?;
    let mut rows = connection
        .query(
            "SELECT pointer_json FROM schema_active_pointers
             WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3",
            params![tenant, SCOPE_KIND_TASK, scope.id.as_str()],
        )
        .await
        .map_err(backend)?;
    let Some(row) = rows.next().await.map_err(backend)? else {
        return Ok(None);
    };
    let json: String = row.get(0).map_err(backend)?;
    decode(&json).map(Some)
}
