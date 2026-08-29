use std::collections::BTreeMap;

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
    if let Some(publication_fence) = command.stream_publication_fence.as_ref() {
        let StreamPublicationFence::TaskScoped {
            source_bundle_digest,
            expected_write_version,
            ..
        } = publication_fence
        else {
            return Err(SchemaDeploymentStoreError::InvalidInput(
                "installed-application fence cannot activate a task bundle".into(),
            ));
        };
        if expected_predecessor != Some(source_bundle_digest.as_str()) {
            return Err(SchemaDeploymentStoreError::PredecessorMismatch);
        }
        let suffix = scoped_journal_pin_suffix(&SchemaExecutionPin {
            scope: scope.clone(),
            bundle_digest: source_bundle_digest.clone(),
        });
        let mut generation_rows = tx
            .query(
                "SELECT COUNT(*) FROM events
                 WHERE tenant = ?1 AND substr(entity_id, -length(?2)) = ?2",
                params![tenant, suffix],
            )
            .await
            .map_err(backend)?;
        let write_version = if let Some(row) = generation_rows.next().await.map_err(backend)? {
            let count = row.get::<i64>(0).map_err(backend)?;
            u64::try_from(count).map_err(|_| backend("invalid stream publication generation"))?
        } else {
            0
        };
        drop(generation_rows);
        if write_version != *expected_write_version {
            return Err(SchemaDeploymentStoreError::StaleFence);
        }
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
        stream_fenced_source_bundle_digest: command.stream_publication_fence.as_ref().and_then(
            |fence| match fence {
                StreamPublicationFence::TaskScoped {
                    source_bundle_digest,
                    ..
                } => Some(source_bundle_digest.clone()),
                StreamPublicationFence::InstalledApplication { .. } => None,
            },
        ),
        stream_publication_bindings: command.stream_publication_fence.as_ref().map_or_else(
            BTreeMap::new,
            |fence| match fence {
                StreamPublicationFence::TaskScoped { bindings, .. } => bindings.clone(),
                StreamPublicationFence::InstalledApplication { .. } => BTreeMap::new(),
            },
        ),
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
