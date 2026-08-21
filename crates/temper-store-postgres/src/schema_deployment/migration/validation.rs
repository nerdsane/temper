use super::*;
pub(super) use crate::schema_deployment::helpers::validate_digest;

pub(crate) async fn validate(
    store: &PostgresEventStore,
    tenant: &str,
    job_id: &str,
    expected_fence: u64,
    receipt: SchemaMigrationValidationReceipt,
) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
    validate_text("validation receipt", &receipt.id)?;
    validate_digest("migration shadow digest", &receipt.shadow_digest)?;
    let mut connection = store.pool().acquire().await.map_err(backend)?;
    let mut tx = connection.begin().await.map_err(backend)?;
    let mut job = locked_job(&mut tx, tenant, job_id)
        .await?
        .ok_or(SchemaDeploymentStoreError::NotFound)?;
    let prior = load_receipt::<SchemaMigrationValidationReceipt>(
        &mut tx,
        "schema_migration_validation_receipts",
        tenant,
        job_id,
        &receipt.id,
    )
    .await?;
    if prior.as_ref().is_some_and(|prior| prior != &receipt) {
        return Err(SchemaDeploymentStoreError::MigrationRejected);
    }
    if job.status == SchemaMigrationStatus::Rejected
        && job.validation_receipt_id.as_deref() == Some(receipt.id.as_str())
        && prior.is_some()
    {
        tx.commit().await.map_err(backend)?;
        return Ok(job);
    }
    if job.fence != expected_fence {
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    if receipt.passed {
        if job.status != SchemaMigrationStatus::Validating || !job.scan_complete {
            return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
        }
    } else if !matches!(
        job.status,
        SchemaMigrationStatus::Migrating | SchemaMigrationStatus::Validating
    ) {
        return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
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
    if receipt.passed
        && (u64::try_from(current_write_version).ok() != Some(receipt.caught_up_sequence)
            || receipt.caught_up_sequence != job.catch_up_sequence)
    {
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
    if prior.is_none() {
        sqlx::query(
            "INSERT INTO schema_migration_validation_receipts
             (tenant, job_id, receipt_id, receipt_json) VALUES ($1, $2, $3, $4)",
        )
        .bind(tenant)
        .bind(job_id)
        .bind(&receipt.id)
        .bind(encode(&receipt)?)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
    }
    job.status = if receipt.passed {
        SchemaMigrationStatus::Ready
    } else {
        SchemaMigrationStatus::Rejected
    };
    job.validation_receipt_id = Some(receipt.id);
    if !receipt.passed {
        job.migration_receipt_id = Some(format!("migration-rejected:{job_id}"));
    }
    job.committed_sequence = checked_add(job.committed_sequence, "migration sequence")?;
    write_job(&mut tx, &job).await?;
    tx.commit().await.map_err(backend)?;
    Ok(job)
}

pub(super) fn validate_create(
    command: &CreateSchemaMigration,
) -> Result<(), SchemaDeploymentStoreError> {
    for (name, value) in [
        ("migration job id", command.job_id.as_str()),
        ("tenant", command.tenant.as_str()),
        ("scope id", command.scope.id.as_str()),
        ("source digest", command.source_bundle_digest.as_str()),
        ("target digest", command.target_bundle_digest.as_str()),
        (
            "verification receipt",
            command.verification_receipt_id.as_str(),
        ),
        ("module name", command.module_name.as_str()),
        ("module digest", command.module_digest.as_str()),
        (
            "accepted authority",
            command.accepted_authority_json.as_str(),
        ),
        ("idempotency key", command.idempotency_key.as_str()),
        ("request digest", command.request_digest.as_str()),
        ("request id", command.request_id.as_str()),
    ] {
        validate_text(name, value)?;
    }
    validate_digest("source digest", &command.source_bundle_digest)?;
    validate_digest("target digest", &command.target_bundle_digest)?;
    validate_digest("module digest", &command.module_digest)?;
    validate_digest("request digest", &command.request_digest)?;
    let b = &command.budgets;
    if b.fuel_per_entity == 0
        || b.memory_pages == 0
        || b.input_bytes == 0
        || b.output_bytes == 0
        || b.entities_per_batch == 0
        || b.total_entities == 0
        || b.total_batches == 0
        || b.attempts == 0
    {
        return Err(SchemaDeploymentStoreError::InvalidInput(
            "migration budgets must be positive".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_batch(
    command: &CommitSchemaMigrationBatch,
) -> Result<(), SchemaDeploymentStoreError> {
    validate_text("job id", &command.job_id)?;
    validate_text("batch receipt", &command.receipt.id)?;
    validate_text("batch input digest", &command.receipt.input_digest)?;
    validate_text("batch output digest", &command.receipt.output_digest)?;
    validate_digest("batch input digest", &command.receipt.input_digest)?;
    validate_digest("batch output digest", &command.receipt.output_digest)?;
    let mut previous = command.expected_cursor.clone();
    for row in &command.rows {
        validate_text("entity type", &row.entity_type)?;
        validate_text("entity id", &row.entity_id)?;
        validate_text("canonical state", &row.canonical_state_json)?;
        validate_text("input digest", &row.input_digest)?;
        validate_text("output digest", &row.output_digest)?;
        validate_digest("input digest", &row.input_digest)?;
        validate_digest("output digest", &row.output_digest)?;
        if row.target_event.sequence_nr == 0
            || row.target_event.event_type.trim().is_empty()
            || !row.target_event.payload.is_object()
        {
            return Err(SchemaDeploymentStoreError::MigrationRejected);
        }
        if previous.as_ref().is_some_and(|cursor| {
            (row.entity_type.as_str(), row.entity_id.as_str())
                <= (cursor.0.as_str(), cursor.1.as_str())
        }) {
            return Err(SchemaDeploymentStoreError::MigrationRejected);
        }
        previous = Some((row.entity_type.clone(), row.entity_id.clone()));
    }
    if command.restart_scan && (command.scan_complete || command.next_cursor.is_some()) {
        return Err(SchemaDeploymentStoreError::MigrationRejected);
    }
    if !command.restart_scan
        && let Some(last) = command.rows.last()
        && command.next_cursor.as_ref() != Some(&(last.entity_type.clone(), last.entity_id.clone()))
    {
        return Err(SchemaDeploymentStoreError::MigrationRejected);
    }
    Ok(())
}

pub(super) fn validate_batch_against_job(
    job: &SchemaMigrationJob,
    command: &CommitSchemaMigrationBatch,
) -> Result<(), SchemaDeploymentStoreError> {
    if job.status != SchemaMigrationStatus::Migrating {
        return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
    }
    if job.fence != command.expected_fence {
        return Err(SchemaDeploymentStoreError::StaleFence);
    }
    if job.scan_cursor != command.expected_cursor
        || command.receipt.source_cursor != command.expected_cursor
        || command.receipt.next_cursor != command.next_cursor
        || command.receipt.row_count as usize != command.rows.len()
    {
        return Err(SchemaDeploymentStoreError::MigrationRejected);
    }
    let count = command.rows.len() as u64;
    if count > u64::from(job.command.budgets.entities_per_batch)
        || job
            .consumed_entities
            .checked_add(count)
            .is_none_or(|value| value > job.command.budgets.total_entities)
        || job
            .consumed_batches
            .checked_add(1)
            .is_none_or(|value| value > job.command.budgets.total_batches)
    {
        return Err(SchemaDeploymentStoreError::MigrationBudgetExhausted);
    }
    Ok(())
}

pub(super) fn validate_text(name: &str, value: &str) -> Result<(), SchemaDeploymentStoreError> {
    let budget = if name.contains("canonical state") || name.contains("accepted authority") {
        1_048_576
    } else if name.contains("digest") {
        128
    } else {
        256
    };
    if value.is_empty() || value.trim() != value || value.len() > budget {
        Err(SchemaDeploymentStoreError::InvalidInput(format!(
            "{name} must be non-empty, canonical, and at most {budget} bytes"
        )))
    } else {
        Ok(())
    }
}

pub(super) fn checked_add(value: u64, name: &str) -> Result<u64, SchemaDeploymentStoreError> {
    value
        .checked_add(1)
        .ok_or_else(|| backend(format!("{name} exhausted")))
}
