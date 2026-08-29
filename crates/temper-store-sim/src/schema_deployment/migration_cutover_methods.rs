macro_rules! impl_schema_migration_cutover_methods {
    () => {
        async fn validate_schema_migration(
            &self,
            tenant: &str,
            job_id: &str,
            expected_fence: u64,
            receipt: SchemaMigrationValidationReceipt,
        ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
            validate_text("migration validation receipt", &receipt.id)?;
            validate_digest("migration shadow digest", &receipt.shadow_digest)?;
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::ValidateMigration)?;
            let receipt_key = (tenant.to_string(), job_id.to_string(), receipt.id.clone());
            if let Some(existing) = inner
                .schema_deployments
                .migration_validation_receipts
                .get(&receipt_key)
                && existing != &receipt
            {
                return Err(SchemaDeploymentStoreError::MigrationRejected);
            }
            let job_snapshot = inner
                .schema_deployments
                .migrations
                .get(&(tenant.to_string(), job_id.to_string()))
                .cloned()
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if job_snapshot.status == SchemaMigrationStatus::Rejected
                && job_snapshot.validation_receipt_id.as_deref() == Some(receipt.id.as_str())
            {
                return Ok(job_snapshot);
            }
            if job_snapshot.fence != expected_fence {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            let suffix = temper_runtime::persistence::schema_deployment::scoped_journal_pin_suffix(
                &temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
                    scope: job_snapshot.command.scope.clone(),
                    bundle_digest: job_snapshot.command.source_bundle_digest.clone(),
                },
            );
            let current_write_version = inner
                .journals
                .iter()
                .filter(|(persistence_id, _)| {
                    temper_runtime::tenant::parse_persistence_id_parts(persistence_id).is_ok_and(
                        |(found_tenant, _, entity_id)| {
                            found_tenant == tenant && entity_id.ends_with(&suffix)
                        },
                    )
                })
                .try_fold(0_u64, |version, (_, events)| {
                    version
                        .checked_add(events.len() as u64)
                        .ok_or(SchemaDeploymentStoreError::MigrationBudgetExhausted)
                })?;
            if receipt.passed
                && (receipt.caught_up_sequence != job_snapshot.catch_up_sequence
                    || receipt.caught_up_sequence != current_write_version)
            {
                let next_sequence =
                    checked_next(job_snapshot.committed_sequence, "migration sequence")?;
                let resumed = inner
                    .schema_deployments
                    .migrations
                    .get_mut(&(tenant.to_string(), job_id.to_string()))
                    .ok_or(SchemaDeploymentStoreError::NotFound)?;
                resumed.status = SchemaMigrationStatus::Migrating;
                resumed.scan_cursor = None;
                resumed.scan_complete = false;
                resumed.catch_up_sequence = current_write_version;
                resumed.validation_receipt_id = None;
                resumed.committed_sequence = next_sequence;
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            if !receipt.passed
                && !matches!(
                    job_snapshot.status,
                    SchemaMigrationStatus::Migrating | SchemaMigrationStatus::Validating
                )
            {
                return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
            }
            let job = inner
                .schema_deployments
                .migrations
                .get_mut(&(tenant.to_string(), job_id.to_string()))
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if receipt.passed
                && (job.status != SchemaMigrationStatus::Validating || !job.scan_complete)
            {
                return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
            }
            let next_sequence = checked_next(job.committed_sequence, "migration sequence")?;
            job.status = if receipt.passed {
                SchemaMigrationStatus::Ready
            } else {
                SchemaMigrationStatus::Rejected
            };
            job.validation_receipt_id = Some(receipt.id.clone());
            if !receipt.passed {
                job.migration_receipt_id = Some(format!("migration-rejected:{job_id}"));
            }
            job.committed_sequence = next_sequence;
            let result = job.clone();
            inner
                .schema_deployments
                .migration_validation_receipts
                .insert(receipt_key, receipt);
            Ok(result)
        }

        async fn cut_over_schema_migration(
            &self,
            tenant: &str,
            job_id: &str,
            expected_fence: u64,
            validation_receipt_id: &str,
        ) -> Result<SchemaActivePointer, SchemaDeploymentStoreError> {
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::CutOverMigration)?;
            let job_key = (tenant.to_string(), job_id.to_string());
            let job = inner
                .schema_deployments
                .migrations
                .get(&job_key)
                .cloned()
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if job.status != SchemaMigrationStatus::Ready
                || job.validation_receipt_id.as_deref() != Some(validation_receipt_id)
            {
                return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
            }
            if job.fence != expected_fence {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            let suffix = temper_runtime::persistence::schema_deployment::scoped_journal_pin_suffix(
                &temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
                    scope: job.command.scope.clone(),
                    bundle_digest: job.command.source_bundle_digest.clone(),
                },
            );
            let current_write_version = inner
                .journals
                .iter()
                .filter(|(persistence_id, _)| {
                    temper_runtime::tenant::parse_persistence_id_parts(persistence_id).is_ok_and(
                        |(found_tenant, _, entity_id)| {
                            found_tenant == tenant && entity_id.ends_with(&suffix)
                        },
                    )
                })
                .try_fold(0_u64, |version, (_, events)| {
                    version
                        .checked_add(events.len() as u64)
                        .ok_or(SchemaDeploymentStoreError::MigrationBudgetExhausted)
                })?;
            if current_write_version != job.catch_up_sequence {
                let next_sequence = checked_next(job.committed_sequence, "migration sequence")?;
                let resumed = inner
                    .schema_deployments
                    .migrations
                    .get_mut(&job_key)
                    .ok_or(SchemaDeploymentStoreError::NotFound)?;
                resumed.status = SchemaMigrationStatus::Migrating;
                resumed.scan_cursor = None;
                resumed.scan_complete = false;
                resumed.catch_up_sequence = current_write_version;
                resumed.validation_receipt_id = None;
                resumed.committed_sequence = next_sequence;
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            let validation_key = (
                tenant.to_string(),
                job_id.to_string(),
                validation_receipt_id.to_string(),
            );
            if !inner
                .schema_deployments
                .migration_validation_receipts
                .get(&validation_key)
                .is_some_and(|receipt| receipt.passed)
            {
                return Err(SchemaDeploymentStoreError::MigrationRejected);
            }
            let scope_key = (tenant.to_string(), job.command.scope.clone());
            let source_pointer = inner
                .schema_deployments
                .active
                .get(&scope_key)
                .ok_or(SchemaDeploymentStoreError::PredecessorMismatch)?;
            if source_pointer.bundle_digest != job.command.source_bundle_digest {
                return Err(SchemaDeploymentStoreError::PredecessorMismatch);
            }
            if source_pointer.fence != job.command.source_expected_fence {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            let target_key = deployment_key(
                tenant,
                &job.command.scope,
                &job.command.target_bundle_digest,
            );
            let target = inner
                .schema_deployments
                .deployments
                .get(&target_key)
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if target.status != SchemaDeploymentStatus::Verified
                || target.verification_receipt_id.as_deref()
                    != Some(job.command.verification_receipt_id.as_str())
            {
                return Err(SchemaDeploymentStoreError::VerificationFailed);
            }
            let next_target_fence = checked_next(target.fence, "migration cutover fence")?;
            let next_target_sequence =
                checked_next(target.committed_sequence, "deployment sequence")?;
            let source_key = deployment_key(
                tenant,
                &job.command.scope,
                &job.command.source_bundle_digest,
            );
            let next_source_sequence = inner
                .schema_deployments
                .deployments
                .get(&source_key)
                .map(|source| checked_next(source.committed_sequence, "deployment sequence"))
                .transpose()?;
            let next_job_sequence = checked_next(job.committed_sequence, "migration sequence")?;

            let target = inner
                .schema_deployments
                .deployments
                .get_mut(&target_key)
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            target.status = SchemaDeploymentStatus::Active;
            target.fence = next_target_fence;
            target.committed_sequence = next_target_sequence;
            let pointer = SchemaActivePointer {
                tenant: tenant.to_string(),
                scope: job.command.scope.clone(),
                bundle_digest: job.command.target_bundle_digest.clone(),
                predecessor_digest: Some(job.command.source_bundle_digest.clone()),
                stream_fenced_source_bundle_digest: None,
                stream_publication_bindings: BTreeMap::new(),
                fence: target.fence,
                committed_sequence: target.committed_sequence,
                accepted_request_id: job.command.request_id.clone(),
            };
            if let (Some(source), Some(next_sequence)) = (
                inner.schema_deployments.deployments.get_mut(&source_key),
                next_source_sequence,
            ) {
                source.status = SchemaDeploymentStatus::Retired;
                source.committed_sequence = next_sequence;
            }
            inner
                .schema_deployments
                .active
                .insert(scope_key, pointer.clone());
            let job = inner
                .schema_deployments
                .migrations
                .get_mut(&job_key)
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            job.status = SchemaMigrationStatus::CutOver;
            job.migration_receipt_id = Some(format!("migration:{}", job.command.job_id));
            job.committed_sequence = next_job_sequence;
            Ok(pointer)
        }

        async fn page_schema_migration_shadow(
            &self,
            tenant: &str,
            job_id: &str,
            after: Option<(&str, &str)>,
            limit: usize,
        ) -> Result<Vec<SchemaMigrationShadowRow>, SchemaDeploymentStoreError> {
            if limit == 0 {
                return Err(SchemaDeploymentStoreError::InvalidInput(
                    "shadow page budget must be positive".into(),
                ));
            }
            let inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            Ok(inner
                .schema_deployments
                .migration_shadow
                .iter()
                .filter(|((found_tenant, found_job, entity_type, entity_id), _)| {
                    found_tenant == tenant
                        && found_job == job_id
                        && after.is_none_or(|cursor| {
                            (entity_type.as_str(), entity_id.as_str()) > cursor
                        })
                })
                .map(|(_, row)| row.clone())
                .take(limit)
                .collect())
        }

        async fn complete_schema_migration(
            &self,
            tenant: &str,
            job_id: &str,
            expected_fence: u64,
        ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
            let mut inner = self.inner.lock().map_err(|_| {
                SchemaDeploymentStoreError::BackendUnavailable("lock poisoned".into())
            })?;
            inject_schema_failure(&mut inner, SimSchemaFaultPoint::CompleteMigration)?;
            let job = inner
                .schema_deployments
                .migrations
                .get_mut(&(tenant.to_string(), job_id.to_string()))
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
            if job.status == SchemaMigrationStatus::Completed {
                return Ok(job.clone());
            }
            if job.status != SchemaMigrationStatus::CutOver {
                return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
            }
            if job.fence != expected_fence {
                return Err(SchemaDeploymentStoreError::StaleFence);
            }
            let next_sequence = checked_next(job.committed_sequence, "migration sequence")?;
            job.status = SchemaMigrationStatus::Completed;
            job.committed_sequence = next_sequence;
            Ok(job.clone())
        }
    };
}
