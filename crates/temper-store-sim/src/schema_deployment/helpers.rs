use super::*;

pub(super) fn validate_digest(name: &str, value: &str) -> Result<(), SchemaDeploymentStoreError> {
    if !temper_runtime::persistence::schema_deployment::is_canonical_sha256_digest(value) {
        return Err(SchemaDeploymentStoreError::InvalidInput(format!(
            "{name} must use canonical sha256:<64 lowercase hex> form"
        )));
    }
    Ok(())
}

pub(super) fn validate_migration_command(
    command: &CreateSchemaMigration,
) -> Result<(), SchemaDeploymentStoreError> {
    for (name, value) in [
        ("migration job id", command.job_id.as_str()),
        ("tenant", command.tenant.as_str()),
        ("scope id", command.scope.id.as_str()),
        (
            "source bundle digest",
            command.source_bundle_digest.as_str(),
        ),
        (
            "target bundle digest",
            command.target_bundle_digest.as_str(),
        ),
        (
            "verification receipt",
            command.verification_receipt_id.as_str(),
        ),
        ("migration module name", command.module_name.as_str()),
        ("migration module digest", command.module_digest.as_str()),
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
    validate_digest("source bundle digest", &command.source_bundle_digest)?;
    validate_digest("target bundle digest", &command.target_bundle_digest)?;
    validate_digest("migration module digest", &command.module_digest)?;
    validate_digest("request digest", &command.request_digest)?;
    let budgets = &command.budgets;
    if budgets.fuel_per_entity == 0
        || budgets.memory_pages == 0
        || budgets.input_bytes == 0
        || budgets.output_bytes == 0
        || budgets.entities_per_batch == 0
        || budgets.total_entities == 0
        || budgets.total_batches == 0
        || budgets.attempts == 0
    {
        return Err(SchemaDeploymentStoreError::InvalidInput(
            "migration budgets must be positive".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_migration_batch(
    command: &CommitSchemaMigrationBatch,
) -> Result<(), SchemaDeploymentStoreError> {
    validate_text("migration job id", &command.job_id)?;
    validate_text("migration batch receipt", &command.receipt.id)?;
    validate_text(
        "migration batch input digest",
        &command.receipt.input_digest,
    )?;
    validate_text(
        "migration batch output digest",
        &command.receipt.output_digest,
    )?;
    validate_digest(
        "migration batch input digest",
        &command.receipt.input_digest,
    )?;
    validate_digest(
        "migration batch output digest",
        &command.receipt.output_digest,
    )?;
    let mut previous = command.expected_cursor.clone();
    for row in &command.rows {
        validate_text("migration entity type", &row.entity_type)?;
        validate_text("migration entity id", &row.entity_id)?;
        validate_text("migration canonical state", &row.canonical_state_json)?;
        validate_text("migration input digest", &row.input_digest)?;
        validate_text("migration output digest", &row.output_digest)?;
        validate_digest("migration input digest", &row.input_digest)?;
        validate_digest("migration output digest", &row.output_digest)?;
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
