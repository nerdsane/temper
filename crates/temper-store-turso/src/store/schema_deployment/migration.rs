//! Turso migration-job transactions kept separate from deployment lifecycle code.

mod finalize;
mod query;
mod read_write;
mod retry;
mod validation;

pub(super) use finalize::{complete, page_shadow};
pub(super) use query::{claim, get, get_in_scope, list_incomplete};
use read_write::*;
pub(super) use retry::reserve_retry;
pub(super) use validation::validate;
use validation::*;

use libsql::{TransactionBehavior, params};
use temper_runtime::persistence::schema_deployment::{
    CommitSchemaMigrationBatch, CreateSchemaMigration, CreateSchemaMigrationOutcome,
    ReserveSchemaMigrationRetry, SchemaActivePointer, SchemaDeploymentStatus,
    SchemaDeploymentStoreError, SchemaMigrationBatchReceipt, SchemaMigrationJob,
    SchemaMigrationRetryReservation, SchemaMigrationShadowRow, SchemaMigrationStatus,
    SchemaMigrationValidationReceipt, SchemaScope,
};

use super::{
    SCOPE_KIND_TASK, TursoEventStore, backend, decode, encode, load_deployment, write_deployment,
};
use crate::store::write_gate::WritePriority;

pub(super) async fn create(
    store: &TursoEventStore,
    command: CreateSchemaMigration,
) -> Result<CreateSchemaMigrationOutcome, SchemaDeploymentStoreError> {
    validate_create(&command)?;
    let _permit = store
        .acquire_write_permit("schema_migration_create", WritePriority::High)
        .await
        .map_err(backend)?;
    let connection = store.configured_connection().await.map_err(backend)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(backend)?;
    let mut rows = tx
        .query(
            "SELECT request_digest, job_id FROM schema_migration_idempotency
             WHERE tenant = ?1 AND idempotency_key = ?2",
            params![command.tenant.as_str(), command.idempotency_key.as_str()],
        )
        .await
        .map_err(backend)?;
    if let Some(row) = rows.next().await.map_err(backend)? {
        let request_digest: String = row.get(0).map_err(backend)?;
        if request_digest != command.request_digest {
            return Err(SchemaDeploymentStoreError::IdempotencyConflict);
        }
        let job_id: String = row.get(1).map_err(backend)?;
        drop(rows);
        let job = load_job(&tx, &command.tenant, &job_id)
            .await?
            .ok_or_else(|| backend("migration idempotency record lost its job"))?;
        tx.commit().await.map_err(backend)?;
        return Ok(CreateSchemaMigrationOutcome::Replayed(job));
    }
    drop(rows);
    let pointer = load_pointer(&tx, &command.tenant, &command.scope.id)
        .await?
        .ok_or(SchemaDeploymentStoreError::PredecessorMismatch)?;
    if pointer.bundle_digest != command.source_bundle_digest {
        return Err(SchemaDeploymentStoreError::PredecessorMismatch);
    }
    if pointer.fence != command.source_expected_fence {
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    let target = load_deployment(
        &tx,
        &command.tenant,
        &command.scope,
        &command.target_bundle_digest,
    )
    .await?
    .ok_or(SchemaDeploymentStoreError::NotFound)?;
    if target.status != SchemaDeploymentStatus::Verified
        || target.verification_receipt_id.as_deref()
            != Some(command.verification_receipt_id.as_str())
    {
        return Err(SchemaDeploymentStoreError::VerificationFailed);
    }
    if target.bundle.predecessor_digest.as_deref() != Some(command.source_bundle_digest.as_str())
        || target.bundle.migration_module_name.as_deref() != Some(command.module_name.as_str())
        || target.bundle.migration_module_digest.as_deref() != Some(command.module_digest.as_str())
    {
        return Err(SchemaDeploymentStoreError::MigrationRejected);
    }
    if load_job(&tx, &command.tenant, &command.job_id)
        .await?
        .is_some()
    {
        return Err(SchemaDeploymentStoreError::IdempotencyConflict);
    }
    let job = SchemaMigrationJob {
        command: command.clone(),
        status: SchemaMigrationStatus::Submitted,
        fence: 0,
        lease_expires_at: None,
        scan_cursor: None,
        scan_complete: false,
        catch_up_sequence: 0,
        consumed_entities: 0,
        consumed_batches: 0,
        consumed_attempts: 0,
        validation_receipt_id: None,
        migration_receipt_id: None,
        committed_sequence: 1,
    };
    tx.execute(
        "INSERT INTO schema_migration_jobs
         (tenant, job_id, scope_kind, scope_id, job_json) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            command.tenant.as_str(),
            command.job_id.as_str(),
            SCOPE_KIND_TASK,
            command.scope.id.as_str(),
            encode(&job)?
        ],
    )
    .await
    .map_err(backend)?;
    tx.execute(
        "INSERT INTO schema_migration_idempotency
         (tenant, idempotency_key, request_digest, job_id) VALUES (?1, ?2, ?3, ?4)",
        params![
            command.tenant.as_str(),
            command.idempotency_key.as_str(),
            command.request_digest.as_str(),
            command.job_id.as_str()
        ],
    )
    .await
    .map_err(backend)?;
    tx.commit().await.map_err(backend)?;
    Ok(CreateSchemaMigrationOutcome::Created(job))
}

pub(super) async fn commit_batch(
    store: &TursoEventStore,
    tenant: &str,
    command: CommitSchemaMigrationBatch,
) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
    validate_batch(&command)?;
    let _permit = store
        .acquire_write_permit("schema_migration_batch", WritePriority::High)
        .await
        .map_err(backend)?;
    let connection = store.configured_connection().await.map_err(backend)?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(backend)?;
    if let Some(prior) =
        load_batch_receipt(&tx, tenant, &command.job_id, &command.receipt.id).await?
    {
        if prior != command.receipt {
            return Err(SchemaDeploymentStoreError::MigrationRejected);
        }
        let job = load_job(&tx, tenant, &command.job_id)
            .await?
            .ok_or_else(|| backend("migration receipt lost its job"))?;
        tx.commit().await.map_err(backend)?;
        return Ok(job);
    }
    let mut job = load_job(&tx, tenant, &command.job_id)
        .await?
        .ok_or(SchemaDeploymentStoreError::NotFound)?;
    validate_batch_against_job(&job, &command)?;
    for row in &command.rows {
        if let Some(prior) = load_shadow(&tx, tenant, &command.job_id, row).await?
            && (row.source_sequence < prior.source_sequence
                || (row.source_sequence == prior.source_sequence && prior != *row))
        {
            return Err(SchemaDeploymentStoreError::MigrationRejected);
        }
        let target_entity_id =
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                &row.entity_id,
                &temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
                    scope: job.command.scope.clone(),
                    bundle_digest: job.command.target_bundle_digest.clone(),
                },
            );
        let mut sequences = tx
            .query(
                "SELECT COALESCE(MAX(sequence_nr), 0) FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, row.entity_type.as_str(), target_entity_id.as_str()],
            )
            .await
            .map_err(backend)?;
        let current_sequence = sequences
            .next()
            .await
            .map_err(backend)?
            .ok_or_else(|| backend("target sequence query returned no row"))?
            .get::<i64>(0)
            .map_err(backend)?;
        if u64::try_from(current_sequence)
            .ok()
            .and_then(|value| value.checked_add(1))
            != Some(row.target_event.sequence_nr)
        {
            return Err(SchemaDeploymentStoreError::MigrationRejected);
        }
    }
    for row in &command.rows {
        let target_entity_id =
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                &row.entity_id,
                &temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
                    scope: job.command.scope.clone(),
                    bundle_digest: job.command.target_bundle_digest.clone(),
                },
            );
        tx.execute(
            "INSERT INTO events
             (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7)",
            params![
                tenant,
                row.entity_type.as_str(),
                target_entity_id.as_str(),
                i64::try_from(row.target_event.sequence_nr).map_err(backend)?,
                row.target_event.event_type.as_str(),
                serde_json::to_string(&row.target_event.payload).map_err(backend)?,
                serde_json::to_string(&row.target_event.metadata).map_err(backend)?
            ],
        )
        .await
        .map_err(backend)?;
        tx.execute(
            "INSERT INTO event_segments
             (tenant, entity_type, entity_id, segment_index, start_sequence_nr)
             VALUES (?1, ?2, ?3, 0, 1)
             ON CONFLICT(tenant, entity_type, entity_id, segment_index) DO NOTHING",
            params![tenant, row.entity_type.as_str(), target_entity_id.as_str()],
        )
        .await
        .map_err(backend)?;
        tx.execute(
            "UPDATE event_segments
             SET end_sequence_nr = ?4, event_count = ?4 - start_sequence_nr + 1
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND segment_index = 0",
            params![
                tenant,
                row.entity_type.as_str(),
                target_entity_id.as_str(),
                i64::try_from(row.target_event.sequence_nr).map_err(backend)?
            ],
        )
        .await
        .map_err(backend)?;
        tx.execute(
            "INSERT INTO schema_migration_shadow
             (tenant, job_id, entity_type, entity_id, row_json) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(tenant, job_id, entity_type, entity_id)
             DO UPDATE SET row_json = excluded.row_json",
            params![
                tenant,
                command.job_id.as_str(),
                row.entity_type.as_str(),
                row.entity_id.as_str(),
                encode(row)?
            ],
        )
        .await
        .map_err(backend)?;
    }
    let row_count = command.rows.len() as u64;
    job.scan_cursor = command.next_cursor;
    job.scan_complete = command.scan_complete;
    job.consumed_entities += row_count;
    job.consumed_batches += 1;
    job.catch_up_sequence = command.observed_source_write_version;
    job.committed_sequence = checked_add(job.committed_sequence, "migration sequence")?;
    if job.scan_complete {
        job.status = SchemaMigrationStatus::Validating;
        job.lease_expires_at = None;
    }
    write_job(&tx, &job).await?;
    tx.execute(
        "INSERT INTO schema_migration_batch_receipts
         (tenant, job_id, receipt_id, receipt_json) VALUES (?1, ?2, ?3, ?4)",
        params![
            tenant,
            command.job_id.as_str(),
            command.receipt.id.as_str(),
            encode(&command.receipt)?
        ],
    )
    .await
    .map_err(backend)?;
    tx.commit().await.map_err(backend)?;
    Ok(job)
}

pub(super) async fn cut_over(
    store: &TursoEventStore,
    tenant: &str,
    job_id: &str,
    expected_fence: u64,
    validation_receipt_id: &str,
) -> Result<SchemaActivePointer, SchemaDeploymentStoreError> {
    let _permit = store
        .acquire_write_permit("schema_migration_cutover", WritePriority::High)
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
    if job.status != SchemaMigrationStatus::Ready
        || job.validation_receipt_id.as_deref() != Some(validation_receipt_id)
    {
        return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
    }
    if job.fence != expected_fence {
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    if !load_validation_receipt(&tx, tenant, job_id, validation_receipt_id)
        .await?
        .is_some_and(|receipt| receipt.passed)
    {
        return Err(SchemaDeploymentStoreError::MigrationRejected);
    }
    let source_pointer = load_pointer(&tx, tenant, &job.command.scope.id)
        .await?
        .ok_or(SchemaDeploymentStoreError::PredecessorMismatch)?;
    if source_pointer.bundle_digest != job.command.source_bundle_digest {
        return Err(SchemaDeploymentStoreError::PredecessorMismatch);
    }
    if source_pointer.fence != job.command.source_expected_fence {
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    let source_suffix = temper_runtime::persistence::schema_deployment::scoped_journal_pin_suffix(
        &temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
            scope: job.command.scope.clone(),
            bundle_digest: job.command.source_bundle_digest.clone(),
        },
    );
    let source_pattern = format!("%{source_suffix}");
    let mut version_rows = tx
        .query(
            "SELECT COUNT(*) FROM events WHERE tenant = ?1 AND entity_id LIKE ?2",
            params![tenant, source_pattern],
        )
        .await
        .map_err(backend)?;
    let current_write_version = version_rows
        .next()
        .await
        .map_err(backend)?
        .ok_or_else(|| backend("schema write version query returned no row"))?
        .get::<i64>(0)
        .map_err(backend)?;
    if u64::try_from(current_write_version).ok() != Some(job.catch_up_sequence) {
        job.status = SchemaMigrationStatus::Migrating;
        job.scan_cursor = None;
        job.scan_complete = false;
        job.catch_up_sequence = u64::try_from(current_write_version)
            .map_err(|_| SchemaDeploymentStoreError::MigrationBudgetExhausted)?;
        job.validation_receipt_id = None;
        job.committed_sequence = checked_add(job.committed_sequence, "migration sequence")?;
        write_job(&tx, &job).await?;
        tx.commit().await.map_err(backend)?;
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    let mut target = load_deployment(
        &tx,
        tenant,
        &job.command.scope,
        &job.command.target_bundle_digest,
    )
    .await?
    .ok_or(SchemaDeploymentStoreError::NotFound)?;
    if target.status != SchemaDeploymentStatus::Verified
        || target.verification_receipt_id.as_deref()
            != Some(job.command.verification_receipt_id.as_str())
    {
        return Err(SchemaDeploymentStoreError::VerificationFailed);
    }
    target.status = SchemaDeploymentStatus::Active;
    target.fence = checked_add(target.fence, "migration cutover fence")?;
    target.committed_sequence = checked_add(target.committed_sequence, "deployment sequence")?;
    write_deployment(&tx, &target).await?;
    let pointer = SchemaActivePointer {
        tenant: tenant.to_string(),
        scope: job.command.scope.clone(),
        bundle_digest: job.command.target_bundle_digest.clone(),
        predecessor_digest: Some(job.command.source_bundle_digest.clone()),
        fence: target.fence,
        committed_sequence: target.committed_sequence,
        accepted_request_id: job.command.request_id.clone(),
    };
    write_pointer(&tx, &pointer).await?;
    if let Some(mut source) = load_deployment(
        &tx,
        tenant,
        &job.command.scope,
        &job.command.source_bundle_digest,
    )
    .await?
    {
        source.status = SchemaDeploymentStatus::Retired;
        source.committed_sequence = checked_add(source.committed_sequence, "deployment sequence")?;
        write_deployment(&tx, &source).await?;
    }
    job.status = SchemaMigrationStatus::CutOver;
    job.migration_receipt_id = Some(format!("migration:{}", job.command.job_id));
    job.committed_sequence = checked_add(job.committed_sequence, "migration sequence")?;
    write_job(&tx, &job).await?;
    tx.commit().await.map_err(backend)?;
    Ok(pointer)
}
