use super::*;

fn validate(command: &ReserveSchemaBootstrap) -> Result<(), SchemaDeploymentStoreError> {
    validate_schema_bootstrap_reservation(command)
        .map_err(SchemaDeploymentStoreError::InvalidInput)?;
    for (name, value, budget) in [
        ("tenant", command.tenant.as_str(), 256),
        ("caller authority", command.caller_authority.as_str(), 128),
        (
            "accepted authority",
            command.accepted_authority_json.as_str(),
            1_048_576,
        ),
        ("idempotency key", command.idempotency_key.as_str(), 256),
        ("request digest", command.request_digest.as_str(), 128),
        ("request id", command.request_id.as_str(), 256),
        (
            "activation request id",
            command.activation_request_id.as_str(),
            256,
        ),
        ("entity type", command.entity_type.as_str(), 256),
        ("entity id", command.entity_id.as_str(), 256),
        (
            "canonical initial fields",
            command.canonical_initial_fields_json.as_str(),
            1_048_576,
        ),
    ] {
        if value.trim().is_empty() || value.trim() != value || value.len() > budget {
            return Err(SchemaDeploymentStoreError::InvalidInput(format!(
                "{name} must be non-empty, canonical, and at most {budget} bytes"
            )));
        }
    }
    validate_digest("caller authority", &command.caller_authority)?;
    validate_digest("request digest", &command.request_digest)
}

async fn load_operation(
    tx: &libsql::Transaction,
    tenant: &str,
    caller_authority: &str,
    idempotency_key: &str,
) -> Result<Option<SchemaBootstrapOperation>, SchemaDeploymentStoreError> {
    let mut rows = tx
        .query(
            "SELECT operation_json FROM schema_bootstrap_operations
             WHERE tenant = ?1 AND caller_authority = ?2 AND idempotency_key = ?3",
            params![tenant, caller_authority, idempotency_key],
        )
        .await
        .map_err(backend)?;
    let Some(row) = rows.next().await.map_err(backend)? else {
        return Ok(None);
    };
    let json: String = row.get(0).map_err(backend)?;
    decode(&json).map(Some)
}

async fn write_operation(
    tx: &libsql::Transaction,
    operation: &SchemaBootstrapOperation,
) -> Result<(), SchemaDeploymentStoreError> {
    tx.execute(
        "UPDATE schema_bootstrap_operations SET operation_json = ?4
         WHERE tenant = ?1 AND caller_authority = ?2 AND idempotency_key = ?3",
        params![
            operation.command.tenant.as_str(),
            operation.command.caller_authority.as_str(),
            operation.command.idempotency_key.as_str(),
            encode(operation)?
        ],
    )
    .await
    .map_err(backend)?;
    Ok(())
}

pub(super) async fn reserve(
    store: &TursoEventStore,
    command: ReserveSchemaBootstrap,
) -> Result<ReserveSchemaBootstrapOutcome, SchemaDeploymentStoreError> {
    validate(&command)?;
    let _permit = store
        .acquire_write_permit("schema_bootstrap_reserve", WritePriority::High)
        .await
        .map_err(backend)?;
    let connection = store.configured_connection().await.map_err(backend)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(backend)?;
    if let Some(existing) = load_operation(
        &tx,
        &command.tenant,
        &command.caller_authority,
        &command.idempotency_key,
    )
    .await?
    {
        if existing.command.request_digest != command.request_digest {
            return Err(SchemaDeploymentStoreError::IdempotencyConflict);
        }
        if existing.status == SchemaBootstrapStatus::Completed
            && existing.creation_sequence.is_none()
        {
            tx.commit().await.map_err(backend)?;
            return Ok(ReserveSchemaBootstrapOutcome::Replayed(existing));
        }
        let mut rows = tx
            .query(
                "SELECT owner_caller_authority, owner_idempotency_key
                 FROM schema_bootstrap_targets
                 WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3
                   AND bundle_digest = ?4 AND entity_type = ?5 AND entity_id = ?6",
                params![
                    command.tenant.as_str(),
                    SCOPE_KIND_TASK,
                    existing.pin.scope.id.as_str(),
                    existing.pin.bundle_digest.as_str(),
                    command.entity_type.as_str(),
                    command.entity_id.as_str()
                ],
            )
            .await
            .map_err(backend)?;
        let owner = rows.next().await.map_err(backend)?;
        let owned = if let Some(row) = owner {
            let caller: String = row.get(0).map_err(backend)?;
            let key: String = row.get(1).map_err(backend)?;
            caller == command.caller_authority && key == command.idempotency_key
        } else {
            false
        };
        if !owned {
            return Err(SchemaDeploymentStoreError::BootstrapTargetConflict);
        }
        tx.commit().await.map_err(backend)?;
        return Ok(ReserveSchemaBootstrapOutcome::Replayed(existing));
    }

    let mut rows = tx
        .query(
            "SELECT pointer_json FROM schema_active_pointers
             WHERE tenant = ?1
               AND json_extract(pointer_json, '$.accepted_request_id') = ?2
             ORDER BY scope_kind, scope_id LIMIT 2",
            params![
                command.tenant.as_str(),
                command.activation_request_id.as_str()
            ],
        )
        .await
        .map_err(backend)?;
    let Some(row) = rows.next().await.map_err(backend)? else {
        return Err(SchemaDeploymentStoreError::NotFound);
    };
    let pointer_json: String = row.get(0).map_err(backend)?;
    if rows.next().await.map_err(backend)?.is_some() {
        return Err(SchemaDeploymentStoreError::InvalidInput(
            "activation request identity is not unique".into(),
        ));
    }
    drop(rows);
    let pointer: SchemaActivePointer = decode(&pointer_json)?;
    let deployment = load_deployment(&tx, &command.tenant, &pointer.scope, &pointer.bundle_digest)
        .await?
        .ok_or(SchemaDeploymentStoreError::NotFound)?;
    if deployment.status != SchemaDeploymentStatus::Active
        || deployment.activation_pointer.as_ref() != Some(&pointer)
    {
        return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
    }
    let pin = SchemaExecutionPin {
        scope: pointer.scope,
        bundle_digest: pointer.bundle_digest,
    };
    let inserted = tx
        .execute(
            "INSERT OR IGNORE INTO schema_bootstrap_targets
             (tenant, scope_kind, scope_id, bundle_digest, entity_type, entity_id,
              owner_caller_authority, owner_idempotency_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                command.tenant.as_str(),
                SCOPE_KIND_TASK,
                pin.scope.id.as_str(),
                pin.bundle_digest.as_str(),
                command.entity_type.as_str(),
                command.entity_id.as_str(),
                command.caller_authority.as_str(),
                command.idempotency_key.as_str()
            ],
        )
        .await
        .map_err(backend)?;
    if inserted != 1 {
        return Err(SchemaDeploymentStoreError::BootstrapTargetConflict);
    }
    let operation = SchemaBootstrapOperation {
        command,
        pin,
        status: SchemaBootstrapStatus::Reserved,
        creation_sequence: None,
        action_failure: None,
        receipt: None,
        committed_sequence: 1,
    };
    tx.execute(
        "INSERT INTO schema_bootstrap_operations
         (tenant, caller_authority, idempotency_key, operation_json)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            operation.command.tenant.as_str(),
            operation.command.caller_authority.as_str(),
            operation.command.idempotency_key.as_str(),
            encode(&operation)?
        ],
    )
    .await
    .map_err(backend)?;
    tx.commit().await.map_err(backend)?;
    Ok(ReserveSchemaBootstrapOutcome::Reserved(operation))
}

pub(super) async fn record_action_failure(
    store: &TursoEventStore,
    command: RecordSchemaBootstrapActionFailure,
) -> Result<SchemaBootstrapOperation, SchemaDeploymentStoreError> {
    validate_schema_bootstrap_failure(&command.failure)
        .map_err(SchemaDeploymentStoreError::InvalidInput)?;
    let _permit = store
        .acquire_write_permit("schema_bootstrap_action_failure", WritePriority::High)
        .await
        .map_err(backend)?;
    let connection = store.configured_connection().await.map_err(backend)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(backend)?;
    let mut operation = load_operation(
        &tx,
        &command.tenant,
        &command.caller_authority,
        &command.idempotency_key,
    )
    .await?
    .ok_or(SchemaDeploymentStoreError::NotFound)?;
    if operation.action_failure.as_ref() == Some(&command.failure)
        || operation.status == SchemaBootstrapStatus::Completed
    {
        tx.commit().await.map_err(backend)?;
        return Ok(operation);
    }
    if operation.status != SchemaBootstrapStatus::Created
        || operation.committed_sequence != command.expected_sequence
    {
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    operation.action_failure = Some(command.failure);
    operation.committed_sequence = operation
        .committed_sequence
        .checked_add(1)
        .ok_or_else(|| backend("bootstrap operation sequence exhausted"))?;
    write_operation(&tx, &operation).await?;
    tx.commit().await.map_err(backend)?;
    Ok(operation)
}

pub(super) async fn get(
    store: &TursoEventStore,
    tenant: &str,
    caller_authority: &str,
    idempotency_key: &str,
) -> Result<Option<SchemaBootstrapOperation>, SchemaDeploymentStoreError> {
    let connection = store.configured_connection().await.map_err(backend)?;
    let mut rows = connection
        .query(
            "SELECT operation_json FROM schema_bootstrap_operations
             WHERE tenant = ?1 AND caller_authority = ?2 AND idempotency_key = ?3",
            params![tenant, caller_authority, idempotency_key],
        )
        .await
        .map_err(backend)?;
    let Some(row) = rows.next().await.map_err(backend)? else {
        return Ok(None);
    };
    let json: String = row.get(0).map_err(backend)?;
    decode(&json).map(Some)
}

pub(super) async fn list_incomplete(
    store: &TursoEventStore,
    limit: usize,
) -> Result<Vec<SchemaBootstrapOperation>, SchemaDeploymentStoreError> {
    if limit == 0 || limit > 1_024 {
        return Err(SchemaDeploymentStoreError::InvalidInput(
            "bootstrap recovery page budget must be between 1 and 1024".into(),
        ));
    }
    let connection = store.configured_connection().await.map_err(backend)?;
    let mut rows = connection
        .query(
            "SELECT operation_json FROM schema_bootstrap_operations
             WHERE json_extract(operation_json, '$.status') <> 'completed'
             ORDER BY tenant, caller_authority, idempotency_key LIMIT ?1",
            params![i64::try_from(limit).map_err(backend)?],
        )
        .await
        .map_err(backend)?;
    let mut operations = Vec::with_capacity(limit);
    while let Some(row) = rows.next().await.map_err(backend)? {
        let json: String = row.get(0).map_err(backend)?;
        operations.push(decode(&json)?);
    }
    Ok(operations)
}

pub(super) async fn record_created(
    store: &TursoEventStore,
    command: RecordSchemaBootstrapCreated,
) -> Result<SchemaBootstrapOperation, SchemaDeploymentStoreError> {
    let _permit = store
        .acquire_write_permit("schema_bootstrap_created", WritePriority::High)
        .await
        .map_err(backend)?;
    let connection = store.configured_connection().await.map_err(backend)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(backend)?;
    let mut operation = load_operation(
        &tx,
        &command.tenant,
        &command.caller_authority,
        &command.idempotency_key,
    )
    .await?
    .ok_or(SchemaDeploymentStoreError::NotFound)?;
    if operation.status == SchemaBootstrapStatus::Created
        && operation.creation_sequence == Some(command.creation_sequence)
    {
        tx.commit().await.map_err(backend)?;
        return Ok(operation);
    }
    if operation.status == SchemaBootstrapStatus::Completed
        && operation.creation_sequence == Some(command.creation_sequence)
    {
        tx.commit().await.map_err(backend)?;
        return Ok(operation);
    }
    if operation.status != SchemaBootstrapStatus::Reserved {
        return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
    }
    if operation.committed_sequence != command.expected_sequence {
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    operation.status = SchemaBootstrapStatus::Created;
    operation.creation_sequence = Some(command.creation_sequence);
    operation.committed_sequence = operation
        .committed_sequence
        .checked_add(1)
        .ok_or_else(|| backend("bootstrap operation sequence exhausted"))?;
    write_operation(&tx, &operation).await?;
    tx.commit().await.map_err(backend)?;
    Ok(operation)
}

pub(super) async fn complete(
    store: &TursoEventStore,
    command: CompleteSchemaBootstrap,
) -> Result<SchemaBootstrapOperation, SchemaDeploymentStoreError> {
    validate_schema_bootstrap_receipt(&command.receipt)
        .map_err(SchemaDeploymentStoreError::InvalidInput)?;
    let _permit = store
        .acquire_write_permit("schema_bootstrap_complete", WritePriority::High)
        .await
        .map_err(backend)?;
    let connection = store.configured_connection().await.map_err(backend)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(backend)?;
    let mut operation = load_operation(
        &tx,
        &command.tenant,
        &command.caller_authority,
        &command.idempotency_key,
    )
    .await?
    .ok_or(SchemaDeploymentStoreError::NotFound)?;
    if operation.status == SchemaBootstrapStatus::Completed {
        tx.commit().await.map_err(backend)?;
        return Ok(operation);
    }
    if operation.committed_sequence != command.expected_sequence
        || command.receipt.pin != operation.pin
        || command.receipt.entity_type != operation.command.entity_type
        || command.receipt.entity_id != operation.command.entity_id
        || command.receipt.creation_sequence != operation.creation_sequence
        || operation.action_failure.is_some() && command.receipt.failure != operation.action_failure
    {
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    let release_target = command.receipt.creation_sequence.is_none();
    operation.status = SchemaBootstrapStatus::Completed;
    operation.receipt = Some(command.receipt);
    operation.committed_sequence = operation
        .committed_sequence
        .checked_add(1)
        .ok_or_else(|| backend("bootstrap operation sequence exhausted"))?;
    write_operation(&tx, &operation).await?;
    if release_target {
        tx.execute(
            "DELETE FROM schema_bootstrap_targets
             WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3
               AND bundle_digest = ?4 AND entity_type = ?5 AND entity_id = ?6
               AND owner_caller_authority = ?7 AND owner_idempotency_key = ?8",
            params![
                operation.command.tenant.as_str(),
                SCOPE_KIND_TASK,
                operation.pin.scope.id.as_str(),
                operation.pin.bundle_digest.as_str(),
                operation.command.entity_type.as_str(),
                operation.command.entity_id.as_str(),
                operation.command.caller_authority.as_str(),
                operation.command.idempotency_key.as_str()
            ],
        )
        .await
        .map_err(backend)?;
    }
    tx.commit().await.map_err(backend)?;
    Ok(operation)
}
