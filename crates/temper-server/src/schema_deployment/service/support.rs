use super::*;

#[cfg(feature = "observe")]
pub(super) async fn verify_bundle(
    state: &ServerState,
    record: &SchemaDeploymentRecord,
) -> Result<bool, ServiceError> {
    let budgets: SchemaBundleBudgetsV1 = serde_json::from_str(&record.bundle.canonical_budgets)
        .map_err(|error| ServiceError::new("verification_failed", error.to_string(), false))?;
    if budgets.verification_steps == 0 {
        return Ok(false);
    }
    if record
        .bundle
        .cedar_policies
        .values()
        .any(|source| source.parse::<cedar_policy::PolicySet>().is_err())
    {
        return Ok(false);
    }
    let tenant = TenantId::new(&record.bundle.tenant);
    for (module_name, digest) in &record.bundle.wasm_module_digests {
        let Some(hash) = digest.strip_prefix("sha256:") else {
            return Ok(false);
        };
        if state
            .ensure_wasm_module_cached(&tenant, module_name, hash)
            .await
            .is_err()
        {
            return Ok(false);
        }
    }
    if let (Some(module_name), Some(digest)) = (
        record.bundle.migration_module_name.as_deref(),
        record.bundle.migration_module_digest.as_deref(),
    ) {
        if record.bundle.migration_abi_version.as_deref() != Some("temper-schema-migration/v1") {
            return Ok(false);
        }
        let Some(hash) = digest.strip_prefix("sha256:") else {
            return Ok(false);
        };
        if state
            .ensure_wasm_module_cached(&tenant, module_name, hash)
            .await
            .is_err()
        {
            return Ok(false);
        }
    }
    let sources = record
        .bundle
        .canonical_ioa
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for source in &sources {
        let automaton = temper_spec::automaton::parse_automaton(source)
            .map_err(|error| ServiceError::new("verification_failed", error.to_string(), false))?;
        let integrations_bound = automaton.integrations.iter().all(|integration| {
            integration.integration_type != "wasm"
                || integration
                    .module
                    .as_ref()
                    .is_some_and(|module| record.bundle.wasm_module_digests.contains_key(module))
        });
        let triggers_bound = automaton
            .actions
            .iter()
            .flat_map(|action| &action.triggers)
            .all(|trigger| {
                trigger.kind != temper_spec::automaton::TriggerKind::Wasm
                    || trigger.module.as_ref().is_some_and(|module| {
                        record.bundle.wasm_module_digests.contains_key(module)
                    })
            });
        if !integrations_bound || !triggers_bound {
            return Ok(false);
        }
    }
    let source_count = u64::try_from(sources.len()).unwrap_or(u64::MAX).max(1);
    let steps_per_source = budgets.verification_steps / source_count;
    // The immutable maximum is divided across Z3 resource units, model
    // states, simulation ticks, and property-test transition steps. No level
    // retains an uncharged dynamic default.
    if steps_per_source < 4 {
        return Ok(false);
    }
    let smt_steps = steps_per_source / 4;
    let model_steps = steps_per_source / 4;
    let model_states = usize::try_from(model_steps)
        .map_err(|error| ServiceError::new("verification_failed", error.to_string(), false))?
        .max(1);
    let sim_ticks = steps_per_source / 4;
    let prop_steps = usize::try_from(steps_per_source - smt_steps - sim_ticks - model_steps)
        .map_err(|error| ServiceError::new("verification_failed", error.to_string(), false))?
        .max(1);
    Ok(sources.into_iter().all(|source| {
        temper_verify::VerificationCascade::from_ioa(&source)
            .with_smt_resource_budget(u32::try_from(smt_steps).unwrap_or(u32::MAX))
            .with_model_state_budget(model_states)
            .with_sim_seeds(1)
            .with_sim_ticks(sim_ticks)
            .with_prop_test_cases(1)
            .with_prop_test_max_steps(prop_steps)
            .run()
            .all_passed
    }))
}

#[cfg(not(feature = "observe"))]
pub(super) async fn verify_bundle(
    _state: &ServerState,
    _record: &SchemaDeploymentRecord,
) -> Result<bool, ServiceError> {
    Err(ServiceError::new(
        "backend_unavailable",
        "the verification cascade is not enabled in this server build",
        true,
    ))
}

#[cfg(feature = "observe")]
pub(super) fn ensure_verification_available() -> Result<(), ServiceError> {
    Ok(())
}

pub(super) fn simulated_millis() -> Result<u64, ServiceError> {
    u64::try_from(sim_now().timestamp_millis()).map_err(|_| {
        ServiceError::new(
            "migration_failed",
            "simulated time is before the supported epoch",
            false,
        )
    })
}

pub(super) fn migration_budget_usize(value: u64, name: &str) -> Result<usize, ServiceError> {
    usize::try_from(value).map_err(|_| {
        ServiceError::new(
            "migration_budget_exhausted",
            format!("{name} exceeds the platform addressable budget"),
            false,
        )
    })
}

#[cfg(not(feature = "observe"))]
pub(super) fn ensure_verification_available() -> Result<(), ServiceError> {
    Err(ServiceError::new(
        "backend_unavailable",
        "the verification cascade is not enabled in this server build",
        true,
    ))
}

pub(super) fn parse_scope(scope: &SchemaScopeV1) -> Result<SchemaScope, ServiceError> {
    if scope.kind != "task" || scope.id.trim().is_empty() || scope.id.trim() != scope.id {
        return Err(ServiceError::new(
            "scope_mismatch",
            "scope must be a canonical non-empty task scope",
            false,
        ));
    }
    Ok(SchemaScope {
        kind: SchemaScopeKind::Task,
        id: scope.id.clone(),
    })
}

pub(super) fn scope_v1(scope: &SchemaScope) -> SchemaScopeV1 {
    SchemaScopeV1 {
        kind: "task".into(),
        id: scope.id.clone(),
    }
}

pub(super) fn to_spec_budgets(value: &SchemaBundleBudgetsV1) -> ScopedBundleBudgets {
    ScopedBundleBudgets {
        verification_steps: value.verification_steps,
        migration_fuel_per_entity: value.migration_fuel_per_entity,
        migration_memory_pages: value.migration_memory_pages,
        migration_input_bytes: value.migration_input_bytes,
        migration_output_bytes: value.migration_output_bytes,
        migration_entities_per_batch: value.migration_entities_per_batch,
        migration_total_entities: value.migration_total_entities,
        migration_total_batches: value.migration_total_batches,
        migration_attempts: value.migration_attempts,
    }
}

pub(super) fn to_migration_budgets(value: &SchemaMigrationBudgetsV1) -> SchemaMigrationBudgets {
    SchemaMigrationBudgets {
        fuel_per_entity: value.fuel_per_entity,
        memory_pages: value.memory_pages,
        input_bytes: value.input_bytes,
        output_bytes: value.output_bytes,
        entities_per_batch: value.entities_per_batch,
        total_entities: value.total_entities,
        total_batches: value.total_batches,
        attempts: value.attempts,
    }
}

pub(super) fn migration_limits(
    budgets: &SchemaMigrationBudgetsV1,
) -> Result<temper_wasm::PureMigrationLimits, ServiceError> {
    let limits = temper_wasm::PureMigrationLimits {
        max_fuel: budgets.fuel_per_entity,
        max_memory_pages: budgets.memory_pages,
        max_input_bytes: budgets.input_bytes,
        max_output_bytes: budgets.output_bytes,
        max_duration: std::time::Duration::from_secs(30),
    };
    if budgets.entities_per_batch == 0
        || budgets.total_entities == 0
        || budgets.total_batches == 0
        || budgets.attempts == 0
        || u64::from(budgets.entities_per_batch) > budgets.total_entities
        || budgets.total_batches > budgets.total_entities
        || u64::from(budgets.attempts) > budgets.total_batches
    {
        return Err(ServiceError::new(
            "migration_budget_exhausted",
            "migration job budgets must be positive and internally consistent",
            false,
        ));
    }
    Ok(limits)
}

pub(super) fn migration_limits_from_job(
    job: &SchemaMigrationJob,
) -> Result<temper_wasm::PureMigrationLimits, ServiceError> {
    migration_limits(&SchemaMigrationBudgetsV1 {
        fuel_per_entity: job.command.budgets.fuel_per_entity,
        memory_pages: job.command.budgets.memory_pages,
        input_bytes: job.command.budgets.input_bytes,
        output_bytes: job.command.budgets.output_bytes,
        entities_per_batch: job.command.budgets.entities_per_batch,
        total_entities: job.command.budgets.total_entities,
        total_batches: job.command.budgets.total_batches,
        attempts: job.command.budgets.attempts,
    })
}

pub(super) fn verify_migration_determinism<'a>(
    engine: &temper_wasm::WasmEngine,
    module_hash: &str,
    source_digest: &str,
    target_digest: &str,
    entity_types: impl Iterator<Item = &'a String>,
    limits: temper_wasm::PureMigrationLimits,
) -> Result<(), ServiceError> {
    for (item_index, entity_type) in entity_types.enumerate() {
        let input = SchemaMigrationInputV1 {
            abi_version: 1,
            source_bundle_digest: source_digest.to_string(),
            target_bundle_digest: target_digest.to_string(),
            entity_type: entity_type.clone(),
            entity_id: "temper-migration-verification".into(),
            source_sequence: 0,
            canonical_state_json: r#"{"Id":"temper-migration-verification"}"#.into(),
            logical_context: SchemaMigrationLogicalContextV1 {
                batch_id: "temper-migration-verification".into(),
                item_index: u32::try_from(item_index).map_err(|_| {
                    ServiceError::new(
                        "migration_budget_exhausted",
                        "verification vector budget exhausted",
                        false,
                    )
                })?,
            },
        };
        let first = engine
            .invoke_pure_migration(module_hash, &input, limits)
            .map_err(ServiceError::from_migration)?;
        let second = engine
            .invoke_pure_migration(module_hash, &input, limits)
            .map_err(ServiceError::from_migration)?;
        if first != second {
            return Err(ServiceError::new(
                "migration_rejected",
                "migration module produced nondeterministic verification output",
                false,
            ));
        }
    }
    Ok(())
}

pub(super) fn migration_request_digest(
    tenant: &str,
    security: &SecurityContext,
    request: &StartSchemaMigrationRequestV1,
) -> Result<String, ServiceError> {
    digest_json(&(
        "start_migration",
        tenant,
        security,
        &request.scope,
        request.source_bundle_digest.as_str(),
        request.target_bundle_digest.as_str(),
        request.verification_receipt_id.as_str(),
        request.expected_fence,
        &request.budgets,
    ))
}

pub(super) fn digest_json(value: &impl serde::Serialize) -> Result<String, ServiceError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| ServiceError::new("migration_rejected", error.to_string(), false))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

pub(super) fn operation_identity(
    idempotency_key: String,
    request_id: String,
    canonical_input: &impl serde::Serialize,
) -> Result<SchemaOperationIdentity, ServiceError> {
    Ok(SchemaOperationIdentity {
        idempotency_key,
        request_digest: digest_json(canonical_input)?,
        request_id,
    })
}

pub(super) fn canonical_json_object(value: &serde_json::Value) -> Result<String, ServiceError> {
    if !value.is_object() {
        return Err(ServiceError::new(
            "migration_rejected",
            "migration state must be a JSON object",
            false,
        ));
    }
    serde_json::to_string(&canonicalize_json(value))
        .map_err(|error| ServiceError::new("migration_rejected", error.to_string(), false))
}

fn canonicalize_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize_json(value)))
                .collect::<BTreeMap<_, _>>();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(canonicalize_json).collect())
        }
        scalar => scalar.clone(),
    }
}

pub(super) fn migration_receipt(job: &SchemaMigrationJob) -> SchemaMigrationReceiptV1 {
    SchemaMigrationReceiptV1 {
        request_id: job.command.request_id.clone(),
        job_id: job.command.job_id.clone(),
        scope: scope_v1(&job.command.scope),
        source_bundle_digest: job.command.source_bundle_digest.clone(),
        target_bundle_digest: job.command.target_bundle_digest.clone(),
        status: migration_status_name(job.status).into(),
        fence: job.fence,
        scan_cursor: job.scan_cursor.clone(),
        consumed_entities: job.consumed_entities,
        consumed_batches: job.consumed_batches,
        validation_receipt_id: job.validation_receipt_id.clone(),
        migration_receipt_id: job.migration_receipt_id.clone(),
        committed_sequence: job.committed_sequence,
    }
}

pub(super) fn emit_schema_lifecycle(
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
    from_status: &str,
    to_status: &str,
    scope: &SchemaScope,
) {
    let params = serde_json::json!({
        "scope_kind": "task",
        "scope_id": scope.id,
    });
    let event =
        temper_observe::wide_event::from_transition(temper_observe::wide_event::TransitionInput {
            tenant,
            entity_type,
            entity_id,
            operation,
            from_status,
            to_status,
            success: true,
            duration_ns: 0,
            params: &params,
            item_count: 1,
            trace_id: "",
        });
    temper_observe::wide_event::emit_span(&event);
    temper_observe::wide_event::emit_metrics(&event);
}

pub(super) fn migration_status_name(status: SchemaMigrationStatus) -> &'static str {
    match status {
        SchemaMigrationStatus::Submitted => "submitted",
        SchemaMigrationStatus::Migrating => "migrating",
        SchemaMigrationStatus::Validating => "validating",
        SchemaMigrationStatus::Ready => "ready",
        SchemaMigrationStatus::CutOver => "cut_over",
        SchemaMigrationStatus::Completed => "completed",
        SchemaMigrationStatus::Rejected => "rejected",
    }
}

pub(super) fn canonical_request_digest(
    bundle: &SchemaBundleRecord,
) -> Result<String, ServiceError> {
    let bytes = serde_json::to_vec(bundle)
        .map_err(|error| ServiceError::new("invalid_bundle", error.to_string(), false))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

pub(super) fn receipt(record: &SchemaDeploymentRecord) -> SchemaDeploymentReceiptV1 {
    SchemaDeploymentReceiptV1 {
        request_id: record.accepted_request_id.clone(),
        scope: scope_v1(&record.bundle.scope),
        bundle_digest: record.bundle.digest.clone(),
        predecessor: record.bundle.predecessor_digest.clone(),
        status: status_name(record.status).into(),
        fence: record.fence,
        verification_receipt_id: record.verification_receipt_id.clone(),
        migration_receipt_id: None,
        committed_sequence: record.committed_sequence,
    }
}

pub(super) fn status_name(status: SchemaDeploymentStatus) -> &'static str {
    match status {
        SchemaDeploymentStatus::Submitted => "submitted",
        SchemaDeploymentStatus::Verifying => "verifying",
        SchemaDeploymentStatus::Verified => "verified",
        SchemaDeploymentStatus::Activating => "activating",
        SchemaDeploymentStatus::Active => "active",
        SchemaDeploymentStatus::Retiring => "retiring",
        SchemaDeploymentStatus::Retired => "retired",
        SchemaDeploymentStatus::Rejected => "rejected",
    }
}

pub(super) fn http_response(result: Result<SchemaDeploymentReceiptV1, ServiceError>) -> Response {
    match result {
        Ok(receipt) => (
            StatusCode::OK,
            axum::Json(SchemaDeploymentResponseV1::Ok { receipt }),
        )
            .into_response(),
        Err(error) => (
            error.status(),
            axum::Json(SchemaDeploymentResponseV1::Error {
                error: error.response,
            }),
        )
            .into_response(),
    }
}

pub(super) fn migration_http_response(
    result: Result<SchemaMigrationReceiptV1, ServiceError>,
) -> Response {
    match result {
        Ok(receipt) => (
            StatusCode::OK,
            axum::Json(SchemaDeploymentResponseV1::Migration { receipt }),
        )
            .into_response(),
        Err(error) => (
            error.status(),
            axum::Json(SchemaDeploymentResponseV1::Error {
                error: error.response,
            }),
        )
            .into_response(),
    }
}
