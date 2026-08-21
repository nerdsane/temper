//! PostgreSQL migration-job transactions kept separate from deployment lifecycle code.

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

use sqlx::{Acquire, Postgres, Row, Transaction};
use temper_runtime::persistence::schema_deployment::{
    CommitSchemaMigrationBatch, CreateSchemaMigration, CreateSchemaMigrationOutcome,
    ReserveSchemaMigrationRetry, SchemaActivePointer, SchemaDeploymentStatus,
    SchemaDeploymentStoreError, SchemaMigrationBatchReceipt, SchemaMigrationJob,
    SchemaMigrationRetryReservation, SchemaMigrationShadowRow, SchemaMigrationStatus,
    SchemaMigrationValidationReceipt, SchemaScope,
};

use super::{
    PostgresEventStore, SCOPE_KIND_TASK, backend, decode, encode, lock_schema_key,
    locked_deployment, write_deployment,
};

pub(super) async fn create(
    store: &PostgresEventStore,
    command: CreateSchemaMigration,
) -> Result<CreateSchemaMigrationOutcome, SchemaDeploymentStoreError> {
    validate_create(&command)?;
    let mut connection = store.pool().acquire().await.map_err(backend)?;
    let mut tx = connection.begin().await.map_err(backend)?;
    lock_schema_key(
        &mut tx,
        "idempotency",
        &[&command.tenant, "migration", &command.idempotency_key],
    )
    .await?;
    let idem = sqlx::query(
        "SELECT request_digest, job_id FROM schema_migration_idempotency
         WHERE tenant = $1 AND idempotency_key = $2 FOR UPDATE",
    )
    .bind(&command.tenant)
    .bind(&command.idempotency_key)
    .fetch_optional(&mut *tx)
    .await
    .map_err(backend)?;
    if let Some(row) = idem {
        if row.get::<String, _>("request_digest") != command.request_digest {
            return Err(SchemaDeploymentStoreError::IdempotencyConflict);
        }
        let job = locked_job(&mut tx, &command.tenant, &row.get::<String, _>("job_id"))
            .await?
            .ok_or_else(|| backend("migration idempotency record lost its job"))?;
        tx.commit().await.map_err(backend)?;
        return Ok(CreateSchemaMigrationOutcome::Replayed(job));
    }
    let pointer = locked_pointer(&mut tx, &command.tenant, &command.scope.id)
        .await?
        .ok_or(SchemaDeploymentStoreError::PredecessorMismatch)?;
    if pointer.bundle_digest != command.source_bundle_digest {
        return Err(SchemaDeploymentStoreError::PredecessorMismatch);
    }
    if pointer.fence != command.source_expected_fence {
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    let target = locked_deployment(
        &mut tx,
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
    if locked_job(&mut tx, &command.tenant, &command.job_id)
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
    sqlx::query(
        "INSERT INTO schema_migration_jobs
         (tenant, job_id, scope_kind, scope_id, job_json) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&command.tenant)
    .bind(&command.job_id)
    .bind(SCOPE_KIND_TASK)
    .bind(&command.scope.id)
    .bind(encode(&job)?)
    .execute(&mut *tx)
    .await
    .map_err(backend)?;
    sqlx::query(
        "INSERT INTO schema_migration_idempotency
         (tenant, idempotency_key, request_digest, job_id) VALUES ($1, $2, $3, $4)",
    )
    .bind(&command.tenant)
    .bind(&command.idempotency_key)
    .bind(&command.request_digest)
    .bind(&command.job_id)
    .execute(&mut *tx)
    .await
    .map_err(backend)?;
    tx.commit().await.map_err(backend)?;
    Ok(CreateSchemaMigrationOutcome::Created(job))
}

pub(super) async fn commit_batch(
    store: &PostgresEventStore,
    tenant: &str,
    command: CommitSchemaMigrationBatch,
) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
    validate_batch(&command)?;
    let mut connection = store.pool().acquire().await.map_err(backend)?;
    let mut tx = connection.begin().await.map_err(backend)?;
    if let Some(prior) = load_receipt::<SchemaMigrationBatchReceipt>(
        &mut tx,
        "schema_migration_batch_receipts",
        tenant,
        &command.job_id,
        &command.receipt.id,
    )
    .await?
    {
        if prior != command.receipt {
            return Err(SchemaDeploymentStoreError::MigrationRejected);
        }
        let job = locked_job(&mut tx, tenant, &command.job_id)
            .await?
            .ok_or_else(|| backend("migration receipt lost its job"))?;
        tx.commit().await.map_err(backend)?;
        return Ok(job);
    }
    let mut job = locked_job(&mut tx, tenant, &command.job_id)
        .await?
        .ok_or(SchemaDeploymentStoreError::NotFound)?;
    validate_batch_against_job(&job, &command)?;
    for row in &command.rows {
        let prior = sqlx::query(
            "SELECT row_json FROM schema_migration_shadow
             WHERE tenant = $1 AND job_id = $2 AND entity_type = $3 AND entity_id = $4 FOR UPDATE",
        )
        .bind(tenant)
        .bind(&command.job_id)
        .bind(&row.entity_type)
        .bind(&row.entity_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;
        if let Some(prior) = prior {
            let prior: SchemaMigrationShadowRow = decode(prior.get("row_json"))?;
            if row.source_sequence < prior.source_sequence
                || (row.source_sequence == prior.source_sequence && prior != *row)
            {
                return Err(SchemaDeploymentStoreError::MigrationRejected);
            }
        }
        let target_entity_id =
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                &row.entity_id,
                &temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
                    scope: job.command.scope.clone(),
                    bundle_digest: job.command.target_bundle_digest.clone(),
                },
            );
        let current_sequence = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(MAX(sequence_nr), 0) FROM events
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(&row.entity_type)
        .bind(&target_entity_id)
        .fetch_one(&mut *tx)
        .await
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
        sqlx::query(
            "INSERT INTO events
             (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata)
             VALUES ($1, $2, $3, $4, 0, $5, $6, $7)",
        )
        .bind(tenant)
        .bind(&row.entity_type)
        .bind(&target_entity_id)
        .bind(i64::try_from(row.target_event.sequence_nr).map_err(backend)?)
        .bind(&row.target_event.event_type)
        .bind(&row.target_event.payload)
        .bind(encode(&row.target_event.metadata)?)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        sqlx::query(
            "INSERT INTO event_segments
             (tenant, entity_type, entity_id, segment_index, start_sequence_nr)
             VALUES ($1, $2, $3, 0, 1)
             ON CONFLICT (tenant, entity_type, entity_id, segment_index) DO NOTHING",
        )
        .bind(tenant)
        .bind(&row.entity_type)
        .bind(&target_entity_id)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        sqlx::query(
            "UPDATE event_segments
             SET end_sequence_nr = $4, event_count = $4 - start_sequence_nr + 1
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND segment_index = 0",
        )
        .bind(tenant)
        .bind(&row.entity_type)
        .bind(&target_entity_id)
        .bind(i64::try_from(row.target_event.sequence_nr).map_err(backend)?)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        sqlx::query(
            "INSERT INTO schema_migration_shadow
             (tenant, job_id, entity_type, entity_id, row_json) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (tenant, job_id, entity_type, entity_id)
             DO UPDATE SET row_json = EXCLUDED.row_json",
        )
        .bind(tenant)
        .bind(&command.job_id)
        .bind(&row.entity_type)
        .bind(&row.entity_id)
        .bind(encode(row)?)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
    }
    job.scan_cursor = command.next_cursor;
    job.scan_complete = command.scan_complete;
    job.consumed_entities += command.rows.len() as u64;
    job.consumed_batches += 1;
    job.catch_up_sequence = command.observed_source_write_version;
    job.committed_sequence = checked_add(job.committed_sequence, "migration sequence")?;
    if job.scan_complete {
        job.status = SchemaMigrationStatus::Validating;
        job.lease_expires_at = None;
    }
    write_job(&mut tx, &job).await?;
    sqlx::query(
        "INSERT INTO schema_migration_batch_receipts
         (tenant, job_id, receipt_id, receipt_json) VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant)
    .bind(&command.job_id)
    .bind(&command.receipt.id)
    .bind(encode(&command.receipt)?)
    .execute(&mut *tx)
    .await
    .map_err(backend)?;
    tx.commit().await.map_err(backend)?;
    Ok(job)
}

pub(super) async fn cut_over(
    store: &PostgresEventStore,
    tenant: &str,
    job_id: &str,
    expected_fence: u64,
    validation_receipt_id: &str,
) -> Result<SchemaActivePointer, SchemaDeploymentStoreError> {
    let mut connection = store.pool().acquire().await.map_err(backend)?;
    let mut tx = connection.begin().await.map_err(backend)?;
    let mut job = locked_job(&mut tx, tenant, job_id)
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
    if !load_receipt::<SchemaMigrationValidationReceipt>(
        &mut tx,
        "schema_migration_validation_receipts",
        tenant,
        job_id,
        validation_receipt_id,
    )
    .await?
    .is_some_and(|receipt| receipt.passed)
    {
        return Err(SchemaDeploymentStoreError::MigrationRejected);
    }
    let source_pointer = locked_pointer(&mut tx, tenant, &job.command.scope.id)
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
    let current_write_version: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE tenant = $1 AND entity_id LIKE $2")
            .bind(tenant)
            .bind(&source_pattern)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?;
    if u64::try_from(current_write_version).ok() != Some(job.catch_up_sequence) {
        job.status = SchemaMigrationStatus::Migrating;
        job.scan_cursor = None;
        job.scan_complete = false;
        job.catch_up_sequence = u64::try_from(current_write_version)
            .map_err(|_| SchemaDeploymentStoreError::MigrationBudgetExhausted)?;
        job.validation_receipt_id = None;
        job.committed_sequence = checked_add(job.committed_sequence, "migration sequence")?;
        write_job(&mut tx, &job).await?;
        tx.commit().await.map_err(backend)?;
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    let mut target = locked_deployment(
        &mut tx,
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
    write_deployment(&mut tx, &target).await?;
    let pointer = SchemaActivePointer {
        tenant: tenant.to_string(),
        scope: job.command.scope.clone(),
        bundle_digest: job.command.target_bundle_digest.clone(),
        predecessor_digest: Some(job.command.source_bundle_digest.clone()),
        fence: target.fence,
        committed_sequence: target.committed_sequence,
        accepted_request_id: job.command.request_id.clone(),
    };
    sqlx::query(
        "UPDATE schema_active_pointers SET pointer_json = $4
         WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3",
    )
    .bind(tenant)
    .bind(SCOPE_KIND_TASK)
    .bind(&job.command.scope.id)
    .bind(encode(&pointer)?)
    .execute(&mut *tx)
    .await
    .map_err(backend)?;
    if let Some(mut source) = locked_deployment(
        &mut tx,
        tenant,
        &job.command.scope,
        &job.command.source_bundle_digest,
    )
    .await?
    {
        source.status = SchemaDeploymentStatus::Retired;
        source.committed_sequence = checked_add(source.committed_sequence, "deployment sequence")?;
        write_deployment(&mut tx, &source).await?;
    }
    job.status = SchemaMigrationStatus::CutOver;
    job.migration_receipt_id = Some(format!("migration:{}", job.command.job_id));
    job.committed_sequence = checked_add(job.committed_sequence, "migration sequence")?;
    write_job(&mut tx, &job).await?;
    tx.commit().await.map_err(backend)?;
    Ok(pointer)
}
