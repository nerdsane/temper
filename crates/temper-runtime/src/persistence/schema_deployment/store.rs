use super::*;

/// Semantic atomic transactions required for schema deployment lifecycle.
pub trait SchemaDeploymentStore: Send + Sync + 'static {
    /// Atomically insert an immutable bundle, lifecycle row, and idempotency map.
    fn submit_schema_bundle(
        &self,
        command: SubmitSchemaBundle,
    ) -> impl std::future::Future<
        Output = Result<SubmitSchemaBundleOutcome, SchemaDeploymentStoreError>,
    > + Send;

    /// Read one deployment by exact tenant, scope, and immutable digest.
    fn get_schema_deployment(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        digest: &str,
    ) -> impl std::future::Future<
        Output = Result<Option<SchemaDeploymentRecord>, SchemaDeploymentStoreError>,
    > + Send;

    /// Claim or reclaim bounded verification work and advance its fence.
    fn claim_schema_verification(
        &self,
        command: ClaimSchemaVerification,
    ) -> impl std::future::Future<
        Output = Result<ClaimSchemaVerificationOutcome, SchemaDeploymentStoreError>,
    > + Send;

    /// Atomically commit the verifier receipt and terminal verification state.
    fn finish_schema_verification(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        digest: &str,
        expected_fence: u64,
        receipt: SchemaVerificationReceipt,
    ) -> impl std::future::Future<
        Output = Result<SchemaDeploymentRecord, SchemaDeploymentStoreError>,
    > + Send;

    /// Atomically compare predecessor/fence/receipt and replace the active pointer.
    fn activate_schema_bundle(
        &self,
        command: ActivateSchemaBundle,
    ) -> impl std::future::Future<
        Output = Result<ActivateSchemaBundleOutcome, SchemaDeploymentStoreError>,
    > + Send;

    /// Atomically retire the current active bundle and remove its active pointer.
    fn retire_schema_bundle(
        &self,
        command: RetireSchemaBundle,
    ) -> impl std::future::Future<
        Output = Result<RetireSchemaBundleOutcome, SchemaDeploymentStoreError>,
    > + Send;

    /// Read the complete current pointer or explicit absence atomically.
    fn active_schema_pointer(
        &self,
        tenant: &str,
        scope: &SchemaScope,
    ) -> impl std::future::Future<
        Output = Result<Option<SchemaActivePointer>, SchemaDeploymentStoreError>,
    > + Send;

    /// Atomically create one immutable migration job and idempotency mapping.
    fn create_schema_migration(
        &self,
        command: CreateSchemaMigration,
    ) -> impl std::future::Future<
        Output = Result<CreateSchemaMigrationOutcome, SchemaDeploymentStoreError>,
    > + Send {
        let _ = command;
        async {
            Err(SchemaDeploymentStoreError::BackendUnavailable(
                "schema migration storage is unavailable".into(),
            ))
        }
    }

    /// Read one migration by exact tenant and stable job identity.
    fn get_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
    ) -> impl std::future::Future<
        Output = Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError>,
    > + Send {
        let _ = (tenant, job_id);
        async { Ok(None) }
    }

    /// Read one migration only when its tenant, scope, and identity all match.
    fn get_schema_migration_in_scope(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        job_id: &str,
    ) -> impl std::future::Future<
        Output = Result<Option<SchemaMigrationJob>, SchemaDeploymentStoreError>,
    > + Send {
        async move {
            Ok(self
                .get_schema_migration(tenant, job_id)
                .await?
                .filter(|job| &job.command.scope == scope))
        }
    }

    /// List durable nonterminal migrations in stable identity order.
    fn list_incomplete_schema_migrations(
        &self,
        limit: usize,
    ) -> impl std::future::Future<
        Output = Result<Vec<SchemaMigrationJob>, SchemaDeploymentStoreError>,
    > + Send {
        let _ = limit;
        async { Ok(Vec::new()) }
    }

    /// Atomically reserve or replay one migration retry idempotency key.
    fn reserve_schema_migration_retry(
        &self,
        command: ReserveSchemaMigrationRetry,
    ) -> impl std::future::Future<
        Output = Result<SchemaMigrationRetryReservation, SchemaDeploymentStoreError>,
    > + Send {
        let _ = command;
        async {
            Err(SchemaDeploymentStoreError::BackendUnavailable(
                "schema migration storage is unavailable".into(),
            ))
        }
    }

    /// Claim or reclaim one bounded migration batch and advance its fence.
    fn claim_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
        logical_now: u64,
        lease_expires_at: u64,
    ) -> impl std::future::Future<Output = Result<SchemaMigrationJob, SchemaDeploymentStoreError>> + Send
    {
        let _ = (tenant, job_id, logical_now, lease_expires_at);
        async {
            Err(SchemaDeploymentStoreError::BackendUnavailable(
                "schema migration storage is unavailable".into(),
            ))
        }
    }

    /// Atomically commit transformed rows, cursor, budgets, and replay receipt.
    fn commit_schema_migration_batch(
        &self,
        tenant: &str,
        command: CommitSchemaMigrationBatch,
    ) -> impl std::future::Future<Output = Result<SchemaMigrationJob, SchemaDeploymentStoreError>> + Send
    {
        let _ = (tenant, command);
        async {
            Err(SchemaDeploymentStoreError::BackendUnavailable(
                "schema migration storage is unavailable".into(),
            ))
        }
    }

    /// Atomically record validation, making the job cutover-ready or terminally rejected.
    fn validate_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
        expected_fence: u64,
        receipt: SchemaMigrationValidationReceipt,
    ) -> impl std::future::Future<Output = Result<SchemaMigrationJob, SchemaDeploymentStoreError>> + Send
    {
        let _ = (tenant, job_id, expected_fence, receipt);
        async {
            Err(SchemaDeploymentStoreError::BackendUnavailable(
                "schema migration storage is unavailable".into(),
            ))
        }
    }

    /// Atomically replace the active pointer after complete validated migration.
    fn cut_over_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
        expected_fence: u64,
        validation_receipt_id: &str,
    ) -> impl std::future::Future<Output = Result<SchemaActivePointer, SchemaDeploymentStoreError>> + Send
    {
        let _ = (tenant, job_id, expected_fence, validation_receipt_id);
        async {
            Err(SchemaDeploymentStoreError::BackendUnavailable(
                "schema migration storage is unavailable".into(),
            ))
        }
    }

    /// Mark forward-only post-cutover bookkeeping complete.
    fn complete_schema_migration(
        &self,
        tenant: &str,
        job_id: &str,
        expected_fence: u64,
    ) -> impl std::future::Future<Output = Result<SchemaMigrationJob, SchemaDeploymentStoreError>> + Send
    {
        let _ = (tenant, job_id, expected_fence);
        async {
            Err(SchemaDeploymentStoreError::BackendUnavailable(
                "schema migration storage is unavailable".into(),
            ))
        }
    }

    /// Read ordered shadow rows under an explicit positive page budget.
    fn page_schema_migration_shadow(
        &self,
        tenant: &str,
        job_id: &str,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> impl std::future::Future<
        Output = Result<Vec<SchemaMigrationShadowRow>, SchemaDeploymentStoreError>,
    > + Send {
        let _ = (tenant, job_id, after, limit);
        async { Ok(Vec::new()) }
    }
}
