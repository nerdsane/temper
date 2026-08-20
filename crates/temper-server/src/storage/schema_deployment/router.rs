use super::*;

#[async_trait::async_trait]
impl SchemaDeploymentStoreDyn for TenantStoreRouter {
    async fn submit_schema_bundle(
        &self,
        command: SubmitSchemaBundle,
    ) -> Result<SubmitSchemaBundleOutcome, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(&command.bundle.tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::submit_schema_bundle(&store, command).await
    }

    async fn get_schema_deployment(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        digest: &str,
    ) -> Result<Option<SchemaDeploymentRecord>, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::get_schema_deployment(&store, tenant, scope, digest).await
    }

    async fn claim_schema_verification(
        &self,
        command: ClaimSchemaVerification,
    ) -> Result<ClaimSchemaVerificationOutcome, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(&command.tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::claim_schema_verification(&store, command).await
    }

    async fn finish_schema_verification(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        digest: &str,
        expected_fence: u64,
        receipt: SchemaVerificationReceipt,
    ) -> Result<SchemaDeploymentRecord, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::finish_schema_verification(
            &store,
            tenant,
            scope,
            digest,
            expected_fence,
            receipt,
        )
        .await
    }

    async fn activate_schema_bundle(
        &self,
        command: ActivateSchemaBundle,
    ) -> Result<ActivateSchemaBundleOutcome, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(&command.tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::activate_schema_bundle(&store, command).await
    }

    async fn active_schema_pointer(
        &self,
        tenant: &str,
        scope: &SchemaScope,
    ) -> Result<Option<SchemaActivePointer>, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::active_schema_pointer(&store, tenant, scope).await
    }

    async fn retire_schema_bundle(
        &self,
        command: RetireSchemaBundle,
    ) -> Result<RetireSchemaBundleOutcome, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(&command.tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::retire_schema_bundle(&store, command).await
    }

    async fn create_schema_migration(
        &self,
        command: CreateSchemaMigration,
    ) -> Result<CreateSchemaMigrationOutcome, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(&command.tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::create_schema_migration(&store, command).await
    }

    async fn get_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
    ) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::get_schema_migration(&store, tenant, job_id).await
    }

    async fn get_schema_migration_in_scope(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        job_id: &str,
    ) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::get_schema_migration_in_scope(&store, tenant, scope, job_id).await
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
        let mut tenants = self
            .list_tenants()
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        tenants.push("temper-system".into());
        tenants.sort();
        tenants.dedup();
        let mut jobs = Vec::new();
        for tenant in tenants {
            if jobs.len() == limit {
                break;
            }
            let store = self.store_for_tenant(&tenant).await.map_err(|error| {
                SchemaDeploymentStoreError::BackendUnavailable(error.to_string())
            })?;
            let remaining = limit - jobs.len();
            jobs.extend(
                SchemaDeploymentStore::list_incomplete_schema_migrations(&store, remaining).await?,
            );
        }
        jobs.sort_by(|left, right| {
            (&left.command.tenant, &left.command.job_id)
                .cmp(&(&right.command.tenant, &right.command.job_id))
        });
        jobs.truncate(limit);
        Ok(jobs)
    }

    async fn reserve_schema_migration_retry(
        &self,
        command: ReserveSchemaMigrationRetry,
    ) -> Result<SchemaMigrationRetryReservation, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(&command.tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::reserve_schema_migration_retry(&store, command).await
    }

    async fn claim_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
        logical_now: u64,
        lease_expires_at: u64,
    ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::claim_schema_migration(
            &store,
            tenant,
            job_id,
            logical_now,
            lease_expires_at,
        )
        .await
    }

    async fn commit_schema_migration_batch(
        &self,
        tenant: &str,
        command: CommitSchemaMigrationBatch,
    ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::commit_schema_migration_batch(&store, tenant, command).await
    }

    async fn validate_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
        expected_fence: u64,
        receipt: SchemaMigrationValidationReceipt,
    ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::validate_schema_migration(
            &store,
            tenant,
            job_id,
            expected_fence,
            receipt,
        )
        .await
    }

    async fn cut_over_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
        expected_fence: u64,
        validation_receipt_id: &str,
    ) -> Result<SchemaActivePointer, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::cut_over_schema_migration(
            &store,
            tenant,
            job_id,
            expected_fence,
            validation_receipt_id,
        )
        .await
    }

    async fn page_schema_migration_shadow(
        &self,
        tenant: &str,
        job_id: &str,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<SchemaMigrationShadowRow>, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::page_schema_migration_shadow(&store, tenant, job_id, after, limit)
            .await
    }

    async fn complete_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
        expected_fence: u64,
    ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
        let store = self
            .store_for_tenant(tenant)
            .await
            .map_err(|error| SchemaDeploymentStoreError::BackendUnavailable(error.to_string()))?;
        SchemaDeploymentStore::complete_schema_migration(&store, tenant, job_id, expected_fence)
            .await
    }
}
