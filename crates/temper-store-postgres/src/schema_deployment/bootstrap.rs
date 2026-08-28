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

async fn locked_operation(
    tx: &mut Transaction<'_, Postgres>,
    tenant: &str,
    caller_authority: &str,
    idempotency_key: &str,
) -> Result<Option<SchemaBootstrapOperation>, SchemaDeploymentStoreError> {
    let row = sqlx::query(
        "SELECT operation_json FROM schema_bootstrap_operations
         WHERE tenant = $1 AND caller_authority = $2 AND idempotency_key = $3
         FOR UPDATE",
    )
    .bind(tenant)
    .bind(caller_authority)
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(backend)?;
    row.map(|row| decode(row.get("operation_json"))).transpose()
}

async fn write_operation(
    tx: &mut Transaction<'_, Postgres>,
    operation: &SchemaBootstrapOperation,
) -> Result<(), SchemaDeploymentStoreError> {
    sqlx::query(
        "UPDATE schema_bootstrap_operations SET operation_json = $4
         WHERE tenant = $1 AND caller_authority = $2 AND idempotency_key = $3",
    )
    .bind(&operation.command.tenant)
    .bind(&operation.command.caller_authority)
    .bind(&operation.command.idempotency_key)
    .bind(encode(operation)?)
    .execute(&mut **tx)
    .await
    .map_err(backend)?;
    Ok(())
}

pub(super) async fn reserve(
    store: &PostgresEventStore,
    command: ReserveSchemaBootstrap,
) -> Result<ReserveSchemaBootstrapOutcome, SchemaDeploymentStoreError> {
    validate(&command)?;
    let mut connection = store.pool().acquire().await.map_err(backend)?;
    let mut tx = connection.begin().await.map_err(backend)?;
    lock_schema_key(
        &mut tx,
        "bootstrap-operation",
        &[
            &command.tenant,
            &command.caller_authority,
            &command.idempotency_key,
        ],
    )
    .await?;
    if let Some(existing) = locked_operation(
        &mut tx,
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
        let owner = sqlx::query(
            "SELECT owner_caller_authority, owner_idempotency_key
             FROM schema_bootstrap_targets
             WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3
               AND bundle_digest = $4 AND entity_type = $5 AND entity_id = $6
             FOR UPDATE",
        )
        .bind(&command.tenant)
        .bind(SCOPE_KIND_TASK)
        .bind(&existing.pin.scope.id)
        .bind(&existing.pin.bundle_digest)
        .bind(&command.entity_type)
        .bind(&command.entity_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;
        if !owner.is_some_and(|row| {
            row.get::<String, _>("owner_caller_authority") == command.caller_authority
                && row.get::<String, _>("owner_idempotency_key") == command.idempotency_key
        }) {
            return Err(SchemaDeploymentStoreError::BootstrapTargetConflict);
        }
        tx.commit().await.map_err(backend)?;
        return Ok(ReserveSchemaBootstrapOutcome::Replayed(existing));
    }

    lock_schema_key(
        &mut tx,
        "bootstrap-activation",
        &[&command.tenant, &command.activation_request_id],
    )
    .await?;
    let pointer_rows = sqlx::query(
        "SELECT pointer_json FROM schema_active_pointers
         WHERE tenant = $1 AND pointer_json ->> 'accepted_request_id' = $2
         ORDER BY scope_kind, scope_id FOR UPDATE",
    )
    .bind(&command.tenant)
    .bind(&command.activation_request_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(backend)?;
    if pointer_rows.len() != 1 {
        return if pointer_rows.is_empty() {
            Err(SchemaDeploymentStoreError::NotFound)
        } else {
            Err(SchemaDeploymentStoreError::InvalidInput(
                "activation request identity is not unique".into(),
            ))
        };
    }
    let pointer: SchemaActivePointer = decode(pointer_rows[0].get("pointer_json"))?;
    let deployment = locked_deployment(
        &mut tx,
        &command.tenant,
        &pointer.scope,
        &pointer.bundle_digest,
    )
    .await?
    .ok_or(SchemaDeploymentStoreError::NotFound)?;
    if deployment.status != SchemaDeploymentStatus::Active
        || deployment.activation_pointer.as_ref() != Some(&pointer)
    {
        return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
    }
    lock_schema_key(
        &mut tx,
        "bootstrap-target",
        &[
            &command.tenant,
            &pointer.scope.id,
            &pointer.bundle_digest,
            &command.entity_type,
            &command.entity_id,
        ],
    )
    .await?;
    let pin = SchemaExecutionPin {
        scope: pointer.scope,
        bundle_digest: pointer.bundle_digest,
    };
    let inserted = sqlx::query(
        "INSERT INTO schema_bootstrap_targets
         (tenant, scope_kind, scope_id, bundle_digest, entity_type, entity_id,
          owner_caller_authority, owner_idempotency_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT DO NOTHING",
    )
    .bind(&command.tenant)
    .bind(SCOPE_KIND_TASK)
    .bind(&pin.scope.id)
    .bind(&pin.bundle_digest)
    .bind(&command.entity_type)
    .bind(&command.entity_id)
    .bind(&command.caller_authority)
    .bind(&command.idempotency_key)
    .execute(&mut *tx)
    .await
    .map_err(backend)?;
    if inserted.rows_affected() != 1 {
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
    sqlx::query(
        "INSERT INTO schema_bootstrap_operations
         (tenant, caller_authority, idempotency_key, operation_json)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&operation.command.tenant)
    .bind(&operation.command.caller_authority)
    .bind(&operation.command.idempotency_key)
    .bind(encode(&operation)?)
    .execute(&mut *tx)
    .await
    .map_err(backend)?;
    tx.commit().await.map_err(backend)?;
    Ok(ReserveSchemaBootstrapOutcome::Reserved(operation))
}

pub(super) async fn record_action_failure(
    store: &PostgresEventStore,
    command: RecordSchemaBootstrapActionFailure,
) -> Result<SchemaBootstrapOperation, SchemaDeploymentStoreError> {
    validate_schema_bootstrap_failure(&command.failure)
        .map_err(SchemaDeploymentStoreError::InvalidInput)?;
    let mut connection = store.pool().acquire().await.map_err(backend)?;
    let mut tx = connection.begin().await.map_err(backend)?;
    let mut operation = locked_operation(
        &mut tx,
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
    write_operation(&mut tx, &operation).await?;
    tx.commit().await.map_err(backend)?;
    Ok(operation)
}

pub(super) async fn get(
    store: &PostgresEventStore,
    tenant: &str,
    caller_authority: &str,
    idempotency_key: &str,
) -> Result<Option<SchemaBootstrapOperation>, SchemaDeploymentStoreError> {
    let row = sqlx::query(
        "SELECT operation_json FROM schema_bootstrap_operations
         WHERE tenant = $1 AND caller_authority = $2 AND idempotency_key = $3",
    )
    .bind(tenant)
    .bind(caller_authority)
    .bind(idempotency_key)
    .fetch_optional(store.pool())
    .await
    .map_err(backend)?;
    row.map(|row| decode(row.get("operation_json"))).transpose()
}

pub(super) async fn list_incomplete(
    store: &PostgresEventStore,
    limit: usize,
) -> Result<Vec<SchemaBootstrapOperation>, SchemaDeploymentStoreError> {
    if limit == 0 || limit > 1_024 {
        return Err(SchemaDeploymentStoreError::InvalidInput(
            "bootstrap recovery page budget must be between 1 and 1024".into(),
        ));
    }
    let rows = sqlx::query(
        "SELECT operation_json FROM schema_bootstrap_operations
         WHERE operation_json ->> 'status' <> 'completed'
         ORDER BY tenant, caller_authority, idempotency_key LIMIT $1",
    )
    .bind(i64::try_from(limit).map_err(backend)?)
    .fetch_all(store.pool())
    .await
    .map_err(backend)?;
    rows.into_iter()
        .map(|row| decode(row.get("operation_json")))
        .collect()
}

pub(super) async fn record_created(
    store: &PostgresEventStore,
    command: RecordSchemaBootstrapCreated,
) -> Result<SchemaBootstrapOperation, SchemaDeploymentStoreError> {
    let mut connection = store.pool().acquire().await.map_err(backend)?;
    let mut tx = connection.begin().await.map_err(backend)?;
    let mut operation = locked_operation(
        &mut tx,
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
    write_operation(&mut tx, &operation).await?;
    tx.commit().await.map_err(backend)?;
    Ok(operation)
}

pub(super) async fn complete(
    store: &PostgresEventStore,
    command: CompleteSchemaBootstrap,
) -> Result<SchemaBootstrapOperation, SchemaDeploymentStoreError> {
    validate_schema_bootstrap_receipt(&command.receipt)
        .map_err(SchemaDeploymentStoreError::InvalidInput)?;
    let mut connection = store.pool().acquire().await.map_err(backend)?;
    let mut tx = connection.begin().await.map_err(backend)?;
    let mut operation = locked_operation(
        &mut tx,
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
    write_operation(&mut tx, &operation).await?;
    if release_target {
        sqlx::query(
            "DELETE FROM schema_bootstrap_targets
             WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3
               AND bundle_digest = $4 AND entity_type = $5 AND entity_id = $6
               AND owner_caller_authority = $7 AND owner_idempotency_key = $8",
        )
        .bind(&operation.command.tenant)
        .bind(SCOPE_KIND_TASK)
        .bind(&operation.pin.scope.id)
        .bind(&operation.pin.bundle_digest)
        .bind(&operation.command.entity_type)
        .bind(&operation.command.entity_id)
        .bind(&operation.command.caller_authority)
        .bind(&operation.command.idempotency_key)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
    }
    tx.commit().await.map_err(backend)?;
    Ok(operation)
}
