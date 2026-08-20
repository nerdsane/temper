//! Object-safe schema-deployment storage capability for server services.

mod router;

use temper_runtime::persistence::schema_deployment::{
    ActivateSchemaBundle, ActivateSchemaBundleOutcome, ClaimSchemaVerification,
    ClaimSchemaVerificationOutcome, CommitSchemaMigrationBatch, CreateSchemaMigration,
    CreateSchemaMigrationOutcome, ReserveSchemaMigrationRetry, RetireSchemaBundle,
    RetireSchemaBundleOutcome, SchemaActivePointer, SchemaDeploymentRecord, SchemaDeploymentStore,
    SchemaDeploymentStoreError, SchemaMigrationJob, SchemaMigrationRetryReservation,
    SchemaMigrationShadowRow, SchemaMigrationValidationReceipt, SchemaScope,
    SchemaVerificationReceipt, SubmitSchemaBundle, SubmitSchemaBundleOutcome,
};
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::{TenantStoreRouter, TursoEventStore};

/// Object-safe adapter over the runtime's semantic schema-deployment contract.
#[async_trait::async_trait]
pub trait SchemaDeploymentStoreDyn: Send + Sync {
    async fn submit_schema_bundle(
        &self,
        command: SubmitSchemaBundle,
    ) -> Result<SubmitSchemaBundleOutcome, SchemaDeploymentStoreError>;
    async fn get_schema_deployment(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        digest: &str,
    ) -> Result<Option<SchemaDeploymentRecord>, SchemaDeploymentStoreError>;
    async fn claim_schema_verification(
        &self,
        command: ClaimSchemaVerification,
    ) -> Result<ClaimSchemaVerificationOutcome, SchemaDeploymentStoreError>;
    async fn finish_schema_verification(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        digest: &str,
        expected_fence: u64,
        receipt: SchemaVerificationReceipt,
    ) -> Result<SchemaDeploymentRecord, SchemaDeploymentStoreError>;
    async fn activate_schema_bundle(
        &self,
        command: ActivateSchemaBundle,
    ) -> Result<ActivateSchemaBundleOutcome, SchemaDeploymentStoreError>;
    async fn retire_schema_bundle(
        &self,
        command: RetireSchemaBundle,
    ) -> Result<RetireSchemaBundleOutcome, SchemaDeploymentStoreError>;
    async fn active_schema_pointer(
        &self,
        tenant: &str,
        scope: &SchemaScope,
    ) -> Result<Option<SchemaActivePointer>, SchemaDeploymentStoreError>;
    async fn create_schema_migration(
        &self,
        command: CreateSchemaMigration,
    ) -> Result<CreateSchemaMigrationOutcome, SchemaDeploymentStoreError>;
    async fn get_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
    ) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError>;
    async fn get_schema_migration_in_scope(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        job_id: &str,
    ) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError>;
    async fn list_incomplete_schema_migrations(
        &self,
        limit: usize,
    ) -> Result<Vec<SchemaMigrationJob>, SchemaDeploymentStoreError>;
    async fn reserve_schema_migration_retry(
        &self,
        command: ReserveSchemaMigrationRetry,
    ) -> Result<SchemaMigrationRetryReservation, SchemaDeploymentStoreError>;
    async fn claim_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
        logical_now: u64,
        lease_expires_at: u64,
    ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError>;
    async fn commit_schema_migration_batch(
        &self,
        tenant: &str,
        command: CommitSchemaMigrationBatch,
    ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError>;
    async fn validate_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
        expected_fence: u64,
        receipt: SchemaMigrationValidationReceipt,
    ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError>;
    async fn cut_over_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
        expected_fence: u64,
        validation_receipt_id: &str,
    ) -> Result<SchemaActivePointer, SchemaDeploymentStoreError>;
    async fn page_schema_migration_shadow(
        &self,
        tenant: &str,
        job_id: &str,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<SchemaMigrationShadowRow>, SchemaDeploymentStoreError>;
    async fn complete_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
        expected_fence: u64,
    ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError>;
}

macro_rules! impl_schema_store {
    ($store:ty) => {
        #[async_trait::async_trait]
        impl SchemaDeploymentStoreDyn for $store {
            async fn submit_schema_bundle(
                &self,
                command: SubmitSchemaBundle,
            ) -> Result<SubmitSchemaBundleOutcome, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::submit_schema_bundle(self, command).await
            }
            async fn get_schema_deployment(
                &self,
                tenant: &str,
                scope: &SchemaScope,
                digest: &str,
            ) -> Result<Option<SchemaDeploymentRecord>, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::get_schema_deployment(self, tenant, scope, digest).await
            }
            async fn claim_schema_verification(
                &self,
                command: ClaimSchemaVerification,
            ) -> Result<ClaimSchemaVerificationOutcome, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::claim_schema_verification(self, command).await
            }
            async fn finish_schema_verification(
                &self,
                tenant: &str,
                scope: &SchemaScope,
                digest: &str,
                expected_fence: u64,
                receipt: SchemaVerificationReceipt,
            ) -> Result<SchemaDeploymentRecord, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::finish_schema_verification(
                    self,
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
                SchemaDeploymentStore::activate_schema_bundle(self, command).await
            }
            async fn active_schema_pointer(
                &self,
                tenant: &str,
                scope: &SchemaScope,
            ) -> Result<Option<SchemaActivePointer>, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::active_schema_pointer(self, tenant, scope).await
            }
            async fn retire_schema_bundle(
                &self,
                command: RetireSchemaBundle,
            ) -> Result<RetireSchemaBundleOutcome, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::retire_schema_bundle(self, command).await
            }
            async fn create_schema_migration(
                &self,
                command: CreateSchemaMigration,
            ) -> Result<CreateSchemaMigrationOutcome, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::create_schema_migration(self, command).await
            }
            async fn get_schema_migration(
                &self,
                tenant: &str,
                job_id: &str,
            ) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::get_schema_migration(self, tenant, job_id).await
            }
            async fn get_schema_migration_in_scope(
                &self,
                tenant: &str,
                scope: &SchemaScope,
                job_id: &str,
            ) -> Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::get_schema_migration_in_scope(self, tenant, scope, job_id)
                    .await
            }
            async fn list_incomplete_schema_migrations(
                &self,
                limit: usize,
            ) -> Result<Vec<SchemaMigrationJob>, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::list_incomplete_schema_migrations(self, limit).await
            }
            async fn reserve_schema_migration_retry(
                &self,
                command: ReserveSchemaMigrationRetry,
            ) -> Result<SchemaMigrationRetryReservation, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::reserve_schema_migration_retry(self, command).await
            }
            async fn claim_schema_migration(
                &self,
                tenant: &str,
                job_id: &str,
                logical_now: u64,
                lease_expires_at: u64,
            ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::claim_schema_migration(
                    self,
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
                SchemaDeploymentStore::commit_schema_migration_batch(self, tenant, command).await
            }
            async fn validate_schema_migration(
                &self,
                tenant: &str,
                job_id: &str,
                expected_fence: u64,
                receipt: SchemaMigrationValidationReceipt,
            ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::validate_schema_migration(
                    self,
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
                SchemaDeploymentStore::cut_over_schema_migration(
                    self,
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
                SchemaDeploymentStore::page_schema_migration_shadow(
                    self, tenant, job_id, after, limit,
                )
                .await
            }
            async fn complete_schema_migration(
                &self,
                tenant: &str,
                job_id: &str,
                expected_fence: u64,
            ) -> Result<SchemaMigrationJob, SchemaDeploymentStoreError> {
                SchemaDeploymentStore::complete_schema_migration(
                    self,
                    tenant,
                    job_id,
                    expected_fence,
                )
                .await
            }
        }
    };
}

impl_schema_store!(PostgresEventStore);
impl_schema_store!(TursoEventStore);

#[cfg(feature = "sim")]
impl_schema_store!(temper_store_sim::SimEventStore);
