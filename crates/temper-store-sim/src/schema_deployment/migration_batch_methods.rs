macro_rules! impl_schema_migration_batch_methods {
    () => {
        async fn create_schema_migration(
            &self,
            command: CreateSchemaMigration,
        ) -> Result<CreateSchemaMigrationOutcome, SchemaDeploymentStoreError> {
            validate_migration_command(&command)?;
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::CreateMigration)?;
            let idempotency_key = (command.tenant.clone(), command.idempotency_key.clone());
            if let Some((request_digest, job_id)) = inner
                .schema_deployments
                .migration_idempotency
                .get(&idempotency_key)
            {
                if request_digest != &command.request_digest {
                    return Err(SchemaDeploymentStoreError::IdempotencyConflict);
                }
                let job = inner
                    .schema_deployments
                    .migrations
                    .get(&(command.tenant.clone(), job_id.clone()))
                    .cloned()
                    .ok_or_else(|| {
                        SchemaDeploymentStoreError::BackendUnavailable(
                            "migration idempotency record lost its job".into(),
                        )
                    })?;
                return Ok(CreateSchemaMigrationOutcome::Replayed(job));
            }

            let pointer = inner
                .schema_deployments
                .active
                .get(&(command.tenant.clone(), command.scope.clone()))
                .ok_or(SchemaDeploymentStoreError::PredecessorMismatch)?;
            if pointer.bundle_digest != command.source_bundle_digest {
                return Err(SchemaDeploymentStoreError::PredecessorMismatch);
            }
            if pointer.fence != command.source_expected_fence {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            let target = inner
                .schema_deployments
                .deployments
                .get(&deployment_key(
                    &command.tenant,
                    &command.scope,
                    &command.target_bundle_digest,
                ))
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if target.status != SchemaDeploymentStatus::Verified
                || target.verification_receipt_id.as_deref()
                    != Some(command.verification_receipt_id.as_str())
            {
                return Err(SchemaDeploymentStoreError::VerificationFailed);
            }
            if target.bundle.predecessor_digest.as_deref()
                != Some(command.source_bundle_digest.as_str())
                || target.bundle.migration_module_name.as_deref()
                    != Some(command.module_name.as_str())
                || target.bundle.migration_module_digest.as_deref()
                    != Some(command.module_digest.as_str())
            {
                return Err(SchemaDeploymentStoreError::MigrationRejected);
            }

            let job_key = (command.tenant.clone(), command.job_id.clone());
            if inner.schema_deployments.migrations.contains_key(&job_key) {
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
            inner
                .schema_deployments
                .migrations
                .insert(job_key, job.clone());
            inner
                .schema_deployments
                .migration_idempotency
                .insert(idempotency_key, (command.request_digest, command.job_id));
            Ok(CreateSchemaMigrationOutcome::Created(job))
        }

        async fn get_schema_migration(
            &self,
            tenant: &str,
            job_id: &str,
        ) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
            let inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            Ok(inner
                .schema_deployments
                .migrations
                .get(&(tenant.to_string(), job_id.to_string()))
                .cloned())
        }

        async fn get_schema_migration_in_scope(
            &self,
            tenant: &str,
            scope: &SchemaScope,
            job_id: &str,
        ) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
            let inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            Ok(inner
                .schema_deployments
                .migrations
                .get(&(tenant.to_string(), job_id.to_string()))
                .filter(|job| &job.command.scope == scope)
                .cloned())
        }

        async fn list_incomplete_schema_migrations(
            &self,
            limit: usize,
        ) -> Result<Vec<SchemaMigrationJob>, SchemaDeploymentStoreError> {
            if limit == 0 {
                return Err(SchemaDeploymentStoreError::InvalidInput(
                    "migration list budget must be positive".into(),
                ));
            }
            let inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            Ok(inner
                .schema_deployments
                .migrations
                .values()
                .filter(|job| {
                    !matches!(
                        job.status,
                        SchemaMigrationStatus::Completed | SchemaMigrationStatus::Rejected
                    )
                })
                .take(limit)
                .cloned()
                .collect())
        }

        async fn reserve_schema_migration_retry(
            &self,
            command: ReserveSchemaMigrationRetry,
        ) -> Result<SchemaMigrationRetryReservation, SchemaDeploymentStoreError> {
            validate_text("tenant", &command.tenant)?;
            validate_text("job_id", &command.job_id)?;
            validate_text("idempotency_key", &command.operation.idempotency_key)?;
            validate_text("request_digest", &command.operation.request_digest)?;
            validate_text("request_id", &command.operation.request_id)?;
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::ReserveMigrationRetry)?;
            let key = (
                command.tenant.clone(),
                command.operation.idempotency_key.clone(),
            );
            if let Some((request_digest, job_id, starting_sequence, request_id)) = inner
                .schema_deployments
                .migration_retry_idempotency
                .get(&key)
            {
                if request_digest != &command.operation.request_digest || job_id != &command.job_id
                {
                    return Err(SchemaDeploymentStoreError::IdempotencyConflict);
                }
                let job = inner
                    .schema_deployments
                    .migrations
                    .get(&(command.tenant, command.job_id))
                    .cloned()
                    .ok_or_else(|| {
                        SchemaDeploymentStoreError::BackendUnavailable(
                            "migration retry reservation lost its job".into(),
                        )
                    })?;
                return Ok(SchemaMigrationRetryReservation {
                    job,
                    starting_sequence: *starting_sequence,
                    replayed: true,
                    accepted_request_id: request_id.clone(),
                });
            }
            let job = inner
                .schema_deployments
                .migrations
                .get(&(command.tenant.clone(), command.job_id.clone()))
                .cloned()
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            let starting_sequence = job.committed_sequence;
            inner.schema_deployments.migration_retry_idempotency.insert(
                key,
                (
                    command.operation.request_digest,
                    command.job_id,
                    starting_sequence,
                    command.operation.request_id.clone(),
                ),
            );
            Ok(SchemaMigrationRetryReservation {
                job,
                starting_sequence,
                replayed: false,
                accepted_request_id: command.operation.request_id,
            })
        }

        async fn claim_schema_migration(
            &self,
            tenant: &str,
            job_id: &str,
            logical_now: u64,
            lease_expires_at: u64,
        ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
            if lease_expires_at <= logical_now {
                return Err(SchemaDeploymentStoreError::InvalidInput(
                    "migration lease must end after logical now".into(),
                ));
            }
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::ClaimMigration)?;
            let job = inner
                .schema_deployments
                .migrations
                .get_mut(&(tenant.to_string(), job_id.to_string()))
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            let claimable = job.status == SchemaMigrationStatus::Submitted
                || (job.status == SchemaMigrationStatus::Migrating
                    && job
                        .lease_expires_at
                        .is_some_and(|deadline| deadline <= logical_now));
            if !claimable {
                return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
            }
            if job.consumed_attempts >= job.command.budgets.attempts {
                return Err(SchemaDeploymentStoreError::MigrationBudgetExhausted);
            }
            let next_fence = checked_next(job.fence, "migration fence")?;
            let next_attempts = job
                .consumed_attempts
                .checked_add(1)
                .ok_or(SchemaDeploymentStoreError::MigrationBudgetExhausted)?;
            let next_sequence = checked_next(job.committed_sequence, "migration sequence")?;
            job.status = SchemaMigrationStatus::Migrating;
            job.fence = next_fence;
            job.lease_expires_at = Some(lease_expires_at);
            job.consumed_attempts = next_attempts;
            job.committed_sequence = next_sequence;
            Ok(job.clone())
        }

        async fn commit_schema_migration_batch(
            &self,
            tenant: &str,
            command: CommitSchemaMigrationBatch,
        ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::CommitMigrationBatch)?;
            let receipt_key = (
                tenant.to_string(),
                command.job_id.clone(),
                command.receipt.id.clone(),
            );
            if let Some(existing) = inner
                .schema_deployments
                .migration_batch_receipts
                .get(&receipt_key)
            {
                if existing != &command.receipt {
                    return Err(SchemaDeploymentStoreError::MigrationRejected);
                }
                return inner
                    .schema_deployments
                    .migrations
                    .get(&(tenant.to_string(), command.job_id))
                    .cloned()
                    .ok_or_else(|| {
                        SchemaDeploymentStoreError::BackendUnavailable(
                            "migration receipt lost its job".into(),
                        )
                    });
            }
            validate_migration_batch(&command)?;
            let job_key = (tenant.to_string(), command.job_id.clone());
            let job = inner
                .schema_deployments
                .migrations
                .get(&job_key)
                .cloned()
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if job.status != SchemaMigrationStatus::Migrating {
                return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
            }
            if job.fence != command.expected_fence {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            if job.scan_cursor != command.expected_cursor
                || command.receipt.source_cursor != command.expected_cursor
                || command.receipt.next_cursor != command.next_cursor
            {
                return Err(SchemaDeploymentStoreError::MigrationRejected);
            }
            let row_count = u32::try_from(command.rows.len())
                .map_err(|_| SchemaDeploymentStoreError::MigrationBudgetExhausted)?;
            let next_entities = job
                .consumed_entities
                .checked_add(u64::from(row_count))
                .ok_or(SchemaDeploymentStoreError::MigrationBudgetExhausted)?;
            let next_batches = job
                .consumed_batches
                .checked_add(1)
                .ok_or(SchemaDeploymentStoreError::MigrationBudgetExhausted)?;
            let next_sequence = checked_next(job.committed_sequence, "migration sequence")?;
            if row_count != command.receipt.row_count
                || row_count > job.command.budgets.entities_per_batch
                || next_entities > job.command.budgets.total_entities
                || next_batches > job.command.budgets.total_batches
            {
                return Err(SchemaDeploymentStoreError::MigrationBudgetExhausted);
            }
            for row in &command.rows {
                let key = (
                    tenant.to_string(),
                    command.job_id.clone(),
                    row.entity_type.clone(),
                    row.entity_id.clone(),
                );
                if let Some(existing) = inner.schema_deployments.migration_shadow.get(&key)
                    && (row.source_sequence < existing.source_sequence
                        || (row.source_sequence == existing.source_sequence && existing != row))
                {
                    return Err(SchemaDeploymentStoreError::MigrationRejected);
                }
            }
            for row in &command.rows {
                let persistence_id = format!(
                    "{tenant}:{}:{}",
                    row.entity_type,
                    temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                        &row.entity_id,
                        &temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
                            scope: job.command.scope.clone(),
                            bundle_digest: job.command.target_bundle_digest.clone(),
                        },
                    )
                );
                let current_sequence = inner
                    .journals
                    .get(&persistence_id)
                    .and_then(|events| events.last())
                    .map_or(0, |event| event.sequence_nr);
                if row.target_event.sequence_nr != current_sequence.saturating_add(1) {
                    return Err(SchemaDeploymentStoreError::MigrationRejected);
                }
            }
            for row in &command.rows {
                let persistence_id = format!(
                    "{tenant}:{}:{}",
                    row.entity_type,
                    temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                        &row.entity_id,
                        &temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
                            scope: job.command.scope.clone(),
                            bundle_digest: job.command.target_bundle_digest.clone(),
                        },
                    )
                );
                inner
                    .journals
                    .entry(persistence_id)
                    .or_default()
                    .push(row.target_event.clone());
                inner.schema_deployments.migration_shadow.insert(
                    (
                        tenant.to_string(),
                        command.job_id.clone(),
                        row.entity_type.clone(),
                        row.entity_id.clone(),
                    ),
                    row.clone(),
                );
            }
            let job = inner
                .schema_deployments
                .migrations
                .get_mut(&job_key)
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            job.scan_cursor = command.next_cursor;
            job.scan_complete = command.scan_complete;
            job.consumed_entities = next_entities;
            job.consumed_batches = next_batches;
            job.catch_up_sequence = command.observed_source_write_version;
            job.committed_sequence = next_sequence;
            if job.scan_complete {
                job.status = SchemaMigrationStatus::Validating;
                job.lease_expires_at = None;
            }
            let result = job.clone();
            inner
                .schema_deployments
                .migration_batch_receipts
                .insert(receipt_key, command.receipt);
            inject_schema_failure(
                &mut inner,
                SimSchemaFaultPoint::CommitMigrationBatchResponseLoss,
            )?;
            Ok(result)
        }
    };
}
