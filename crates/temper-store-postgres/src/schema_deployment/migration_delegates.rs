macro_rules! impl_schema_migration_delegates {
    () => {
        async fn create_schema_migration(
            &self,
            command: CreateSchemaMigration,
        ) -> Result<CreateSchemaMigrationOutcome, SchemaDeploymentStoreError> {
            migration::create(self, command).await
        }

        async fn get_schema_migration(
            &self,
            tenant: &str,
            job_id: &str,
        ) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
            migration::get(self, tenant, job_id).await
        }

        async fn get_schema_migration_in_scope(
            &self,
            tenant: &str,
            scope: &SchemaScope,
            job_id: &str,
        ) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
            migration::get_in_scope(self, tenant, scope, job_id).await
        }

        async fn list_incomplete_schema_migrations(
            &self,
            limit: usize,
        ) -> Result<Vec<SchemaMigrationJob>, SchemaDeploymentStoreError> {
            migration::list_incomplete(self, limit).await
        }

        async fn reserve_schema_migration_retry(
            &self,
            command: ReserveSchemaMigrationRetry,
        ) -> Result<SchemaMigrationRetryReservation, SchemaDeploymentStoreError> {
            migration::reserve_retry(self, command).await
        }

        async fn claim_schema_migration(
            &self,
            tenant: &str,
            job_id: &str,
            logical_now: u64,
            lease_expires_at: u64,
        ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
            migration::claim(self, tenant, job_id, logical_now, lease_expires_at).await
        }

        async fn commit_schema_migration_batch(
            &self,
            tenant: &str,
            command: CommitSchemaMigrationBatch,
        ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
            migration::commit_batch(self, tenant, command).await
        }

        async fn validate_schema_migration(
            &self,
            tenant: &str,
            job_id: &str,
            expected_fence: u64,
            receipt: SchemaMigrationValidationReceipt,
        ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
            migration::validate(self, tenant, job_id, expected_fence, receipt).await
        }

        async fn cut_over_schema_migration(
            &self,
            tenant: &str,
            job_id: &str,
            expected_fence: u64,
            validation_receipt_id: &str,
        ) -> Result<SchemaActivePointer, SchemaDeploymentStoreError> {
            migration::cut_over(self, tenant, job_id, expected_fence, validation_receipt_id).await
        }

        async fn page_schema_migration_shadow(
            &self,
            tenant: &str,
            job_id: &str,
            after: Option<(&str, &str)>,
            limit: usize,
        ) -> Result<Vec<SchemaMigrationShadowRow>, SchemaDeploymentStoreError> {
            migration::page_shadow(self, tenant, job_id, after, limit).await
        }

        async fn complete_schema_migration(
            &self,
            tenant: &str,
            job_id: &str,
            expected_fence: u64,
        ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
            migration::complete(self, tenant, job_id, expected_fence).await
        }
    };
}
