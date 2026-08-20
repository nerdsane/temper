use super::*;

impl GovernedSchemaDeploymentService<'_> {
    pub(crate) async fn start_migration(
        &self,
        tenant: &str,
        security: &SecurityContext,
        request: StartSchemaMigrationRequestV1,
    ) -> Result<SchemaMigrationReceiptV1, ServiceError> {
        let scope = parse_scope(&request.scope)?;
        self.authorize(
            tenant,
            security,
            "schema_migration_start",
            &scope,
            Some(&request.target_bundle_digest),
        )
        .await?;
        let target = self
            .store()?
            .get_schema_deployment(tenant, &scope, &request.target_bundle_digest)
            .await
            .map_err(ServiceError::from_store)?
            .ok_or_else(|| ServiceError::new("invalid_bundle", "target bundle not found", false))?;
        if target.bundle.predecessor_digest.as_deref()
            != Some(request.source_bundle_digest.as_str())
            || target.verification_receipt_id.as_deref()
                != Some(request.verification_receipt_id.as_str())
            || target.status != SchemaDeploymentStatus::Verified
        {
            return Err(ServiceError::new(
                "migration_rejected",
                "target predecessor or verification receipt does not match",
                false,
            ));
        }
        let module_name = target.bundle.migration_module_name.clone().ok_or_else(|| {
            ServiceError::new(
                "migration_rejected",
                "target has no migration module",
                false,
            )
        })?;
        let module_digest = target
            .bundle
            .migration_module_digest
            .clone()
            .ok_or_else(|| {
                ServiceError::new(
                    "migration_rejected",
                    "target has no migration digest",
                    false,
                )
            })?;
        if target.bundle.migration_abi_version.as_deref() != Some("temper-schema-migration/v1") {
            return Err(ServiceError::new(
                "migration_rejected",
                "target migration ABI is unsupported",
                false,
            ));
        }
        let bundle_budgets: SchemaBundleBudgetsV1 =
            serde_json::from_str(&target.bundle.canonical_budgets).map_err(|error| {
                ServiceError::new("migration_rejected", error.to_string(), false)
            })?;
        if request.budgets.fuel_per_entity > bundle_budgets.migration_fuel_per_entity
            || request.budgets.memory_pages > bundle_budgets.migration_memory_pages
            || request.budgets.input_bytes > bundle_budgets.migration_input_bytes
            || request.budgets.output_bytes > bundle_budgets.migration_output_bytes
            || request.budgets.entities_per_batch > bundle_budgets.migration_entities_per_batch
            || request.budgets.total_entities > bundle_budgets.migration_total_entities
            || request.budgets.total_batches > bundle_budgets.migration_total_batches
            || request.budgets.attempts > bundle_budgets.migration_attempts
        {
            return Err(ServiceError::new(
                "migration_budget_exhausted",
                "migration request exceeds the immutable target bundle budgets",
                false,
            ));
        }
        let engine_hash = module_digest.strip_prefix("sha256:").ok_or_else(|| {
            ServiceError::new("migration_rejected", "invalid module digest", false)
        })?;
        self.state
            .ensure_wasm_module_cached(&TenantId::new(tenant), &module_name, engine_hash)
            .await
            .map_err(|error| ServiceError::new("migration_rejected", error, false))?;
        let limits = migration_limits(&request.budgets)?;
        self.state
            .wasm_engine
            .verify_pure_migration_module(engine_hash, limits)
            .map_err(ServiceError::from_migration)?;
        let runtime_entity_types = target
            .bundle
            .canonical_ioa
            .keys()
            .map(|qualified| {
                qualified
                    .rsplit('.')
                    .next()
                    .unwrap_or(qualified)
                    .to_string()
            })
            .collect::<Vec<_>>();
        verify_migration_determinism(
            &self.state.wasm_engine,
            engine_hash,
            &request.source_bundle_digest,
            &request.target_bundle_digest,
            runtime_entity_types.iter(),
            limits,
        )?;

        let request_digest = migration_request_digest(tenant, security, &request)?;
        let job_id = format!("migration:{}", &request_digest[7..23]);
        let authority = serde_json::to_string(security)
            .map_err(|error| ServiceError::new("migration_rejected", error.to_string(), false))?;
        let outcome = self
            .store()?
            .create_schema_migration(CreateSchemaMigration {
                job_id,
                tenant: tenant.to_string(),
                scope,
                source_bundle_digest: request.source_bundle_digest,
                target_bundle_digest: request.target_bundle_digest,
                verification_receipt_id: request.verification_receipt_id,
                source_expected_fence: request.expected_fence,
                module_name,
                module_digest,
                accepted_authority_json: authority,
                budgets: to_migration_budgets(&request.budgets),
                idempotency_key: request.idempotency_key,
                request_digest,
                request_id: request.request_id,
            })
            .await
            .map_err(ServiceError::from_store)?;
        match outcome {
            CreateSchemaMigrationOutcome::Created(job) => {
                emit_schema_lifecycle(
                    tenant,
                    "SchemaMigration",
                    &job.command.job_id,
                    "create",
                    "absent",
                    "submitted",
                    &job.command.scope,
                );
                self.run_and_supervise_migration(job).await
            }
            CreateSchemaMigrationOutcome::Replayed(job)
                if job.status == SchemaMigrationStatus::Submitted
                    || (job.status == SchemaMigrationStatus::Migrating
                        && job.consumed_batches == 0) =>
            {
                self.run_and_supervise_migration(job).await
            }
            CreateSchemaMigrationOutcome::Replayed(job) => Ok(migration_receipt(&job)),
        }
    }

    async fn run_and_supervise_migration(
        &self,
        job: SchemaMigrationJob,
    ) -> Result<SchemaMigrationReceiptV1, ServiceError> {
        let tenant = job.command.tenant.clone();
        let job_id = job.command.job_id.clone();
        let scope = job.command.scope.clone();
        let receipt = match self.run_migration_batch(job).await {
            Ok(receipt) => receipt,
            Err(error) => {
                return self
                    .persist_terminal_migration_failure(&tenant, &scope, &job_id, error)
                    .await;
            }
        };
        if receipt.status != "completed" {
            let durable = self
                .store()?
                .get_schema_migration(&tenant, &job_id)
                .await
                .map_err(ServiceError::from_store)?
                .ok_or_else(|| {
                    ServiceError::new(
                        "migration_failed",
                        "migration supervisor handoff lost its durable job",
                        true,
                    )
                })?;
            self.state
                .enqueue_schema_migration(durable)
                .map_err(|error| ServiceError::new("backend_unavailable", error, true))?;
        }
        Ok(receipt)
    }

    pub(crate) async fn get_migration(
        &self,
        tenant: &str,
        security: &SecurityContext,
        request: GetSchemaMigrationRequestV1,
    ) -> Result<SchemaMigrationReceiptV1, ServiceError> {
        let scope = parse_scope(&request.scope)?;
        self.authorize(tenant, security, "schema_migration_get", &scope, None)
            .await?;
        let job = self
            .store()?
            .get_schema_migration_in_scope(tenant, &scope, &request.job_id)
            .await
            .map_err(ServiceError::from_store)?
            .ok_or_else(|| ServiceError::new("migration_rejected", "migration not found", false))?;
        let mut receipt = migration_receipt(&job);
        receipt.request_id = request.request_id;
        Ok(receipt)
    }

    pub(crate) async fn retry_migration(
        &self,
        tenant: &str,
        security: &SecurityContext,
        request: RetrySchemaMigrationRequestV1,
    ) -> Result<SchemaMigrationReceiptV1, ServiceError> {
        let scope = parse_scope(&request.scope)?;
        self.authorize(tenant, security, "schema_migration_retry", &scope, None)
            .await?;
        let _job = self
            .store()?
            .get_schema_migration_in_scope(tenant, &scope, &request.job_id)
            .await
            .map_err(ServiceError::from_store)?
            .ok_or_else(|| ServiceError::new("migration_rejected", "migration not found", false))?;
        let operation = operation_identity(
            request.idempotency_key,
            request.request_id.clone(),
            &("retry_migration", tenant, &scope, request.job_id.as_str()),
        )?;
        let reservation = self
            .store()?
            .reserve_schema_migration_retry(ReserveSchemaMigrationRetry {
                tenant: tenant.to_string(),
                job_id: request.job_id,
                operation,
            })
            .await
            .map_err(ServiceError::from_store)?;
        let accepted_request_id = reservation.accepted_request_id.clone();
        let mut receipt = if reservation.replayed
            && reservation.job.committed_sequence > reservation.starting_sequence
        {
            migration_receipt(&reservation.job)
        } else {
            self.drive_migration(reservation.job).await?
        };
        receipt.request_id = accepted_request_id;
        Ok(receipt)
    }
}
