use super::*;

impl GovernedSchemaDeploymentService<'_> {
    pub(crate) async fn drive_migration(
        &self,
        job: SchemaMigrationJob,
    ) -> Result<SchemaMigrationReceiptV1, ServiceError> {
        let tenant = job.command.tenant.clone();
        let scope = job.command.scope.clone();
        let job_id = job.command.job_id.clone();
        match self.drive_migration_attempt(job).await {
            Ok(receipt) => Ok(receipt),
            Err(error) => {
                self.persist_terminal_migration_failure(&tenant, &scope, &job_id, error)
                    .await
            }
        }
    }

    async fn drive_migration_attempt(
        &self,
        mut job: SchemaMigrationJob,
    ) -> Result<SchemaMigrationReceiptV1, ServiceError> {
        let accepted: SecurityContext = serde_json::from_str(&job.command.accepted_authority_json)
            .map_err(|error| ServiceError::new("migration_rejected", error.to_string(), false))?;
        self.authorize(
            &job.command.tenant,
            &accepted,
            "schema_migration_start",
            &job.command.scope,
            Some(&job.command.target_bundle_digest),
        )
        .await?;
        let work_budget = job.command.budgets.total_batches.saturating_add(4);
        for _ in 0..work_budget {
            let prior_sequence = job.committed_sequence;
            let receipt = self.run_migration_batch(job.clone()).await?;
            if receipt.status == "completed" {
                return Ok(receipt);
            }
            job = self
                .store()?
                .get_schema_migration_in_scope(
                    &job.command.tenant,
                    &job.command.scope,
                    &job.command.job_id,
                )
                .await
                .map_err(ServiceError::from_store)?
                .ok_or_else(|| {
                    ServiceError::new(
                        "migration_failed",
                        "migration supervisor lost its durable job",
                        true,
                    )
                })?;
            if job.committed_sequence <= prior_sequence {
                return Err(ServiceError::new(
                    "migration_failed",
                    "migration supervisor made no durable progress",
                    true,
                ));
            }
        }
        Err(ServiceError::new(
            "migration_budget_exhausted",
            "migration supervisor exhausted its durable work budget",
            false,
        ))
    }

    pub(super) async fn persist_terminal_migration_failure(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        job_id: &str,
        error: ServiceError,
    ) -> Result<SchemaMigrationReceiptV1, ServiceError> {
        if error.is_retryable() {
            return Err(error);
        }
        let job = self
            .store()?
            .get_schema_migration_in_scope(tenant, scope, job_id)
            .await
            .map_err(ServiceError::from_store)?
            .ok_or_else(|| ServiceError::new("migration_failed", "migration disappeared", true))?;
        if job.status == SchemaMigrationStatus::Rejected {
            return Ok(migration_receipt(&job));
        }
        if !matches!(
            job.status,
            SchemaMigrationStatus::Migrating | SchemaMigrationStatus::Validating
        ) {
            return Err(error);
        }
        let rejection_digest = digest_json(&serde_json::json!({
            "code": error.code(),
            "message": error.message(),
        }))?;
        let receipt_id = format!("rejection:{}", &rejection_digest[7..23]);
        let from_status = migration_status_name(job.status);
        let rejected = self
            .store()?
            .validate_schema_migration(
                tenant,
                job_id,
                job.fence,
                SchemaMigrationValidationReceipt {
                    id: receipt_id,
                    shadow_digest: rejection_digest,
                    caught_up_sequence: job.catch_up_sequence,
                    passed: false,
                },
            )
            .await
            .map_err(ServiceError::from_store)?;
        emit_schema_lifecycle(
            tenant,
            "SchemaMigration",
            job_id,
            "reject",
            from_status,
            "rejected",
            scope,
        );
        Ok(migration_receipt(&rejected))
    }
}
