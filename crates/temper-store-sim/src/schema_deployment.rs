//! Deterministic schema-deployment transactions for [`SimEventStore`](crate::SimEventStore).

#[macro_use]
mod core_methods;
mod helpers;
#[macro_use]
mod pointer_methods;
#[macro_use]
mod migration_batch_methods;
#[macro_use]
mod migration_cutover_methods;
#[macro_use]
mod retire_methods;

use helpers::*;

use std::collections::BTreeMap;

use temper_runtime::persistence::schema_deployment::{
    ActivateSchemaBundle, ActivateSchemaBundleOutcome, ClaimSchemaVerification,
    ClaimSchemaVerificationOutcome, CommitSchemaMigrationBatch, CreateSchemaMigration,
    CreateSchemaMigrationOutcome, ReserveSchemaMigrationRetry, RetireSchemaBundle,
    RetireSchemaBundleOutcome, SchemaActivePointer, SchemaDeploymentRecord, SchemaDeploymentStatus,
    SchemaDeploymentStore, SchemaDeploymentStoreError, SchemaMigrationBatchReceipt,
    SchemaMigrationJob, SchemaMigrationRetryReservation, SchemaMigrationShadowRow,
    SchemaMigrationStatus, SchemaMigrationValidationReceipt, SchemaOperationIdentity, SchemaScope,
    SchemaVerificationReceipt, SchemaVerificationReplay, StreamPublicationFence,
    SubmitSchemaBundle, SubmitSchemaBundleOutcome,
};
use temper_runtime::persistence::schema_deployment::{
    SchemaExecutionPin, scoped_journal_pin_suffix,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use crate::{SimEventStore, SimEventStoreInner};

/// Deterministic pre-commit failure points for schema lifecycle transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SimSchemaFaultPoint {
    /// Fail while reading the active scope pointer.
    ActivePointerRead,
    /// Fail before an immutable bundle and its idempotency record commit.
    SubmitBundle,
    /// Fail before a verification lease and fence commit.
    ClaimVerification,
    /// Fail before a verification receipt and lifecycle state commit.
    FinishVerification,
    /// Fail before an active scope pointer changes.
    ActivateBundle,
    /// Fail before an active bundle is retired.
    RetireBundle,
    /// Fail before a migration job and its idempotency record commit.
    CreateMigration,
    /// Fail before a migration retry reservation commits.
    ReserveMigrationRetry,
    /// Fail before a migration lease and fence commit.
    ClaimMigration,
    /// Fail before a shadow batch, cursor, and receipt commit.
    CommitMigrationBatch,
    /// Commit a complete batch, then simulate loss of its response.
    CommitMigrationBatchResponseLoss,
    /// Fail before a validation receipt and lifecycle state commit.
    ValidateMigration,
    /// Fail before the source-to-target pointer cutover commits.
    CutOverMigration,
    /// Fail before forward-only migration completion commits.
    CompleteMigration,
}

type DeploymentKey = (String, SchemaScope, String);
type ScopeKey = (String, SchemaScope);
type OperationIdempotencyKey = (String, String, String);
type OperationIdempotencyValue = (String, String, Option<String>);

#[derive(Debug, Default)]
pub(super) struct SimSchemaDeploymentState {
    deployments: BTreeMap<DeploymentKey, SchemaDeploymentRecord>,
    idempotency: BTreeMap<OperationIdempotencyKey, OperationIdempotencyValue>,
    active: BTreeMap<ScopeKey, SchemaActivePointer>,
    verification_receipts:
        BTreeMap<(String, SchemaScope, String, String), SchemaVerificationReceipt>,
    migrations: BTreeMap<(String, String), SchemaMigrationJob>,
    migration_idempotency: BTreeMap<(String, String), (String, String)>,
    migration_retry_idempotency: BTreeMap<(String, String), (String, String, u64, String)>,
    migration_shadow: BTreeMap<(String, String, String, String), SchemaMigrationShadowRow>,
    migration_batch_receipts: BTreeMap<(String, String, String), SchemaMigrationBatchReceipt>,
    migration_validation_receipts:
        BTreeMap<(String, String, String), SchemaMigrationValidationReceipt>,
}

impl SimSchemaDeploymentState {
    pub(super) fn permits_scoped_journal_write(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        digest: &str,
    ) -> bool {
        self.active
            .iter()
            .any(|((found_tenant, found_scope), pointer)| {
                found_tenant == tenant && found_scope == scope && pointer.bundle_digest == digest
            })
            || self.migrations.values().any(|job| {
                job.command.tenant == tenant
                    && &job.command.scope == scope
                    && job.command.target_bundle_digest == digest
                    && matches!(
                        job.status,
                        SchemaMigrationStatus::Submitted
                            | SchemaMigrationStatus::Migrating
                            | SchemaMigrationStatus::Validating
                            | SchemaMigrationStatus::Ready
                    )
            })
    }

    pub(super) fn migrated_source_is_fenced(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        digest: &str,
    ) -> bool {
        self.migrations.values().any(|job| {
            job.command.tenant == tenant
                && &job.command.scope == scope
                && job.command.source_bundle_digest == digest
                && matches!(
                    job.status,
                    SchemaMigrationStatus::CutOver | SchemaMigrationStatus::Completed
                )
        })
    }

    pub(super) fn scoped_stream_publication_action<'a>(
        &'a self,
        tenant: &str,
        scope: &SchemaScope,
        digest: &str,
        entity_type: &str,
    ) -> Option<&'a str> {
        self.active
            .get(&(tenant.to_string(), scope.clone()))
            .filter(|pointer| pointer.stream_fenced_source_bundle_digest.as_deref() == Some(digest))
            .and_then(|pointer| pointer.stream_publication_bindings.get(entity_type))
            .map(String::as_str)
    }
}

fn deployment_key(tenant: &str, scope: &SchemaScope, digest: &str) -> DeploymentKey {
    (tenant.to_string(), scope.clone(), digest.to_string())
}

fn validate_text(name: &str, value: &str) -> Result<(), SchemaDeploymentStoreError> {
    let budget = if name.contains("canonical state") || name.contains("accepted authority") {
        1_048_576
    } else if name.contains("digest") {
        128
    } else {
        256
    };
    if value.trim().is_empty() || value.trim() != value || value.len() > budget {
        return Err(SchemaDeploymentStoreError::InvalidInput(format!(
            "{name} must be non-empty, canonical, and at most {budget} bytes"
        )));
    }
    Ok(())
}

fn validate_operation(
    tenant: &str,
    scope: &SchemaScope,
    digest: &str,
    operation: &SchemaOperationIdentity,
) -> Result<(), SchemaDeploymentStoreError> {
    validate_text("tenant", tenant)?;
    validate_text("scope id", &scope.id)?;
    validate_text("bundle digest", digest)?;
    validate_digest("bundle digest", digest)?;
    validate_text("idempotency key", &operation.idempotency_key)?;
    validate_text("request digest", &operation.request_digest)?;
    validate_text("request id", &operation.request_id)?;
    validate_digest("request digest", &operation.request_digest)
}

fn checked_next(value: u64, name: &str) -> Result<u64, SchemaDeploymentStoreError> {
    value
        .checked_add(1)
        .ok_or_else(|| SchemaDeploymentStoreError::BackendUnavailable(format!("{name} exhausted")))
}

fn inject_schema_failure(
    inner: &mut SimEventStoreInner,
    point: SimSchemaFaultPoint,
) -> Result<(), SchemaDeploymentStoreError> {
    let Some(remaining) = inner.pending_schema_failures.get_mut(&point) else {
        return Ok(());
    };
    if *remaining == 0 {
        inner.pending_schema_failures.remove(&point);
        return Ok(());
    }
    *remaining -= 1;
    if *remaining == 0 {
        inner.pending_schema_failures.remove(&point);
    }
    Err(SchemaDeploymentStoreError::BackendUnavailable(format!(
        "injected schema transaction failure at {point:?}"
    )))
}

impl SimEventStore {
    /// Fail the next `count` transactions at one schema lifecycle commit point.
    ///
    /// Injection occurs before authority state mutates, allowing deterministic
    /// crash/retry tests to prove old-or-new visibility and replay safety.
    pub fn fail_next_schema_operations(&self, point: SimSchemaFaultPoint, count: u64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if count == 0 {
            inner.pending_schema_failures.remove(&point);
        } else {
            inner.pending_schema_failures.insert(point, count);
        }
    }
}

impl SchemaDeploymentStore for SimEventStore {
    impl_schema_core_methods!();
    impl_schema_migration_batch_methods!();
    impl_schema_migration_cutover_methods!();
}
