use super::*;

pub(super) async fn activate(
    store: &TursoEventStore,
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
    let _permit = store
        .acquire_write_permit("schema_bundle_activate", WritePriority::High)
        .await
        .map_err(backend)?;
    let connection = store.configured_connection().await.map_err(backend)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(backend)?;
    if let Some((request_digest, bundle_digest, scope_id)) =
        load_idempotency(&tx, tenant, "activate", &command.operation.idempotency_key).await?
    {
        if request_digest != command.operation.request_digest
            || bundle_digest != command.bundle_digest
            || scope_id != command.scope.id
        {
            return Err(SchemaDeploymentStoreError::IdempotencyConflict);
        }
        let record = load_deployment(&tx, tenant, scope, digest)
            .await?
            .ok_or_else(|| backend("activation idempotency record lost its deployment"))?;
        let pointer = record
            .activation_pointer
            .ok_or_else(|| backend("activation idempotency record lost its receipt"))?;
        tx.commit().await.map_err(backend)?;
        return Ok(ActivateSchemaBundleOutcome::Replayed(pointer));
    }
    let mut rows = tx
        .query(
            "SELECT pointer_json FROM schema_active_pointers
             WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3",
            params![tenant, SCOPE_KIND_TASK, scope.id.as_str()],
        )
        .await
        .map_err(backend)?;
    let active = if let Some(row) = rows.next().await.map_err(backend)? {
        let json: String = row.get(0).map_err(backend)?;
        Some(decode::<SchemaActivePointer>(&json)?)
    } else {
        None
    };
    drop(rows);
    if active
        .as_ref()
        .map(|pointer| pointer.bundle_digest.as_str())
        != expected_predecessor
    {
        return Err(SchemaDeploymentStoreError::PredecessorMismatch);
    }
    let mut record = load_deployment(&tx, tenant, scope, digest)
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
    let mut receipt_rows = tx
        .query(
            "SELECT receipt_json FROM schema_verification_receipts
             WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3
               AND bundle_digest = ?4 AND receipt_id = ?5",
            params![
                tenant,
                SCOPE_KIND_TASK,
                scope.id.as_str(),
                digest,
                verification_receipt_id
            ],
        )
        .await
        .map_err(backend)?;
    let receipt = if let Some(row) = receipt_rows.next().await.map_err(backend)? {
        let json: String = row.get(0).map_err(backend)?;
        Some(decode::<SchemaVerificationReceipt>(&json)?)
    } else {
        None
    };
    drop(receipt_rows);
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
    write_deployment(&tx, &record).await?;
    tx.execute(
        "INSERT INTO schema_active_pointers (tenant, scope_kind, scope_id, pointer_json)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(tenant, scope_kind, scope_id)
         DO UPDATE SET pointer_json = excluded.pointer_json",
        params![
            tenant,
            SCOPE_KIND_TASK,
            scope.id.as_str(),
            encode(&pointer)?
        ],
    )
    .await
    .map_err(backend)?;
    if let Some(predecessor) = expected_predecessor
        && let Some(mut previous) = load_deployment(&tx, tenant, scope, predecessor).await?
    {
        previous.status = SchemaDeploymentStatus::Retired;
        previous.committed_sequence = previous
            .committed_sequence
            .checked_add(1)
            .ok_or_else(|| backend("sequence exhausted"))?;
        write_deployment(&tx, &previous).await?;
    }
    insert_idempotency(&tx, tenant, "activate", &command.operation, scope, digest).await?;
    tx.commit().await.map_err(backend)?;
    Ok(ActivateSchemaBundleOutcome::Activated(pointer))
}
