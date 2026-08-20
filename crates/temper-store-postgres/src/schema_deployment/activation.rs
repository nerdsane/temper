use super::*;

pub(super) async fn activate(
    store: &PostgresEventStore,
    command: ActivateSchemaBundle,
) -> Result<ActivateSchemaBundleOutcome, SchemaDeploymentStoreError> {
    validate_operation(
        &command.tenant,
        &command.scope,
        &command.bundle_digest,
        &command.operation,
    )?;
    if let Some(predecessor) = command.expected_predecessor.as_deref() {
        validate_digest("expected predecessor digest", predecessor)?;
    }
    let tenant = command.tenant.as_str();
    let scope = &command.scope;
    let digest = command.bundle_digest.as_str();
    let expected_predecessor = command.expected_predecessor.as_deref();
    let verification_receipt_id = command.verification_receipt_id.as_str();
    let mut connection = store.pool().acquire().await.map_err(backend)?;
    let mut tx = connection.begin().await.map_err(backend)?;
    lock_schema_key(
        &mut tx,
        "idempotency",
        &[tenant, "activate", &command.operation.idempotency_key],
    )
    .await?;
    if let Some((request_digest, bundle_digest, scope_id)) = locked_idempotency(
        &mut tx,
        tenant,
        "activate",
        &command.operation.idempotency_key,
    )
    .await?
    {
        if request_digest != command.operation.request_digest
            || bundle_digest != command.bundle_digest
            || scope_id != command.scope.id
        {
            return Err(SchemaDeploymentStoreError::IdempotencyConflict);
        }
        let record = locked_deployment(&mut tx, tenant, scope, digest)
            .await?
            .ok_or_else(|| backend("activation idempotency record lost its deployment"))?;
        let pointer = record
            .activation_pointer
            .ok_or_else(|| backend("activation idempotency record lost its receipt"))?;
        tx.commit().await.map_err(backend)?;
        return Ok(ActivateSchemaBundleOutcome::Replayed(pointer));
    }
    lock_schema_key(&mut tx, "scope", &[tenant, SCOPE_KIND_TASK, &scope.id]).await?;
    let active_row = sqlx::query(
        "SELECT pointer_json FROM schema_active_pointers
         WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3 FOR UPDATE",
    )
    .bind(tenant)
    .bind(SCOPE_KIND_TASK)
    .bind(&scope.id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(backend)?;
    let active = active_row
        .map(|row| decode::<SchemaActivePointer>(row.get("pointer_json")))
        .transpose()?;
    if active
        .as_ref()
        .map(|pointer| pointer.bundle_digest.as_str())
        != expected_predecessor
    {
        return Err(SchemaDeploymentStoreError::PredecessorMismatch);
    }
    let mut record = locked_deployment(&mut tx, tenant, scope, digest)
        .await?
        .ok_or(SchemaDeploymentStoreError::NotFound)?;
    if record.status != SchemaDeploymentStatus::Verified {
        return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
    }
    if record.fence != command.expected_fence {
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    if record.bundle.predecessor_digest.as_deref() != expected_predecessor {
        return Err(SchemaDeploymentStoreError::PredecessorMismatch);
    }
    if record.verification_receipt_id.as_deref() != Some(verification_receipt_id) {
        return Err(SchemaDeploymentStoreError::VerificationFailed);
    }
    let receipt_row = sqlx::query(
        "SELECT receipt_json FROM schema_verification_receipts
         WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3
           AND bundle_digest = $4 AND receipt_id = $5",
    )
    .bind(tenant)
    .bind(SCOPE_KIND_TASK)
    .bind(&scope.id)
    .bind(digest)
    .bind(verification_receipt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(backend)?;
    let receipt = receipt_row
        .map(|row| decode::<SchemaVerificationReceipt>(row.get("receipt_json")))
        .transpose()?;
    if !receipt.is_some_and(|receipt| receipt.passed) {
        return Err(SchemaDeploymentStoreError::VerificationFailed);
    }
    record.status = SchemaDeploymentStatus::Active;
    record.fence = record
        .fence
        .checked_add(1)
        .ok_or_else(|| backend("fence exhausted"))?;
    record.committed_sequence = record
        .committed_sequence
        .checked_add(1)
        .ok_or_else(|| backend("sequence exhausted"))?;
    let pointer = SchemaActivePointer {
        tenant: tenant.to_string(),
        scope: scope.clone(),
        bundle_digest: digest.to_string(),
        predecessor_digest: expected_predecessor.map(str::to_string),
        fence: record.fence,
        committed_sequence: record.committed_sequence,
        accepted_request_id: command.operation.request_id.clone(),
    };
    record.activation_pointer = Some(pointer.clone());
    write_deployment(&mut tx, &record).await?;
    sqlx::query(
        "INSERT INTO schema_active_pointers (tenant, scope_kind, scope_id, pointer_json)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (tenant, scope_kind, scope_id)
         DO UPDATE SET pointer_json = EXCLUDED.pointer_json",
    )
    .bind(tenant)
    .bind(SCOPE_KIND_TASK)
    .bind(&scope.id)
    .bind(encode(&pointer)?)
    .execute(&mut *tx)
    .await
    .map_err(backend)?;
    if let Some(predecessor) = expected_predecessor
        && let Some(mut previous) = locked_deployment(&mut tx, tenant, scope, predecessor).await?
    {
        previous.status = SchemaDeploymentStatus::Retired;
        previous.committed_sequence = previous
            .committed_sequence
            .checked_add(1)
            .ok_or_else(|| backend("sequence exhausted"))?;
        write_deployment(&mut tx, &previous).await?;
    }
    insert_idempotency(
        &mut tx,
        tenant,
        "activate",
        &command.operation,
        scope,
        digest,
    )
    .await?;
    tx.commit().await.map_err(backend)?;
    Ok(ActivateSchemaBundleOutcome::Activated(pointer))
}
