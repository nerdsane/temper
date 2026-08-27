//! Turso transactions for ADR-0159 schema deployment lifecycle.

mod activation;
mod helpers;
mod migration;
#[macro_use]
mod migration_delegates;

use libsql::{TransactionBehavior, params};
use temper_runtime::persistence::schema_deployment::{
    ActivateSchemaBundle, ActivateSchemaBundleOutcome, ClaimSchemaVerification,
    ClaimSchemaVerificationOutcome, CommitSchemaMigrationBatch, CreateSchemaMigration,
    CreateSchemaMigrationOutcome, ReserveSchemaMigrationRetry, RetireSchemaBundle,
    RetireSchemaBundleOutcome, SchemaActivePointer, SchemaDeploymentRecord, SchemaDeploymentStatus,
    SchemaDeploymentStore, SchemaDeploymentStoreError, SchemaExecutionPin, SchemaMigrationJob,
    SchemaMigrationRetryReservation, SchemaMigrationShadowRow, SchemaMigrationValidationReceipt,
    SchemaOperationIdentity, SchemaScope, SchemaVerificationReceipt, SchemaVerificationReplay,
    StreamPublicationFence, SubmitSchemaBundle, SubmitSchemaBundleOutcome,
    scoped_journal_pin_suffix,
};

use super::{TursoEventStore, write_gate::WritePriority};
use helpers::*;

const SCOPE_KIND_TASK: &str = "task";

impl SchemaDeploymentStore for TursoEventStore {
    async fn submit_schema_bundle(
        &self,
        command: SubmitSchemaBundle,
    ) -> Result<SubmitSchemaBundleOutcome, SchemaDeploymentStoreError> {
        validate_submit(&command)?;
        let _permit = self
            .acquire_write_permit("schema_bundle_submit", WritePriority::High)
            .await
            .map_err(backend)?;
        let connection = self.configured_connection().await.map_err(backend)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let mut rows = tx
            .query(
                "SELECT request_digest, bundle_digest, scope_id
                 FROM schema_deployment_idempotency
                 WHERE tenant = ?1 AND operation = 'submit' AND idempotency_key = ?2",
                params![
                    command.bundle.tenant.as_str(),
                    command.idempotency_key.as_str()
                ],
            )
            .await
            .map_err(backend)?;
        if let Some(row) = rows.next().await.map_err(backend)? {
            let request_digest: String = row.get(0).map_err(backend)?;
            if request_digest != command.request_digest {
                return Err(SchemaDeploymentStoreError::IdempotencyConflict);
            }
            let digest: String = row.get(1).map_err(backend)?;
            let scope = SchemaScope {
                kind: command.bundle.scope.kind.clone(),
                id: row.get(2).map_err(backend)?,
            };
            drop(rows);
            let record = load_deployment(&tx, &command.bundle.tenant, &scope, &digest)
                .await?
                .ok_or_else(|| backend("idempotency record lost its deployment"))?;
            tx.commit().await.map_err(backend)?;
            return Ok(SubmitSchemaBundleOutcome::Replayed(record));
        }
        drop(rows);

        if let Some(existing) = load_deployment(
            &tx,
            &command.bundle.tenant,
            &command.bundle.scope,
            &command.bundle.digest,
        )
        .await?
        {
            if existing.bundle != command.bundle {
                return Err(SchemaDeploymentStoreError::InvalidInput(
                    "bundle digest aliases different canonical artifacts".into(),
                ));
            }
            tx.execute(
                "INSERT INTO schema_deployment_idempotency
                 (tenant, operation, idempotency_key, request_digest, bundle_digest, scope_kind, scope_id)
                 VALUES (?1, 'submit', ?2, ?3, ?4, ?5, ?6)",
                params![
                    command.bundle.tenant.as_str(),
                    command.idempotency_key.as_str(),
                    command.request_digest.as_str(),
                    command.bundle.digest.as_str(),
                    SCOPE_KIND_TASK,
                    command.bundle.scope.id.as_str()
                ],
            )
            .await
            .map_err(backend)?;
            tx.commit().await.map_err(backend)?;
            return Ok(SubmitSchemaBundleOutcome::Replayed(existing));
        }

        let record = SchemaDeploymentRecord {
            bundle: command.bundle.clone(),
            status: SchemaDeploymentStatus::Submitted,
            fence: 0,
            lease_expires_at: None,
            verification_receipt_id: None,
            verification_replay: None,
            activation_pointer: None,
            committed_sequence: 1,
            accepted_request_id: command.request_id,
            verification_request_id: None,
            retirement_request_id: None,
        };
        tx.execute(
            "INSERT INTO schema_deployments
             (tenant, scope_kind, scope_id, bundle_digest, record_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                command.bundle.tenant.as_str(),
                SCOPE_KIND_TASK,
                command.bundle.scope.id.as_str(),
                command.bundle.digest.as_str(),
                encode(&record)?
            ],
        )
        .await
        .map_err(backend)?;
        tx.execute(
            "INSERT INTO schema_deployment_idempotency
             (tenant, operation, idempotency_key, request_digest, bundle_digest, scope_kind, scope_id)
             VALUES (?1, 'submit', ?2, ?3, ?4, ?5, ?6)",
            params![
                command.bundle.tenant.as_str(),
                command.idempotency_key.as_str(),
                command.request_digest.as_str(),
                command.bundle.digest.as_str(),
                SCOPE_KIND_TASK,
                command.bundle.scope.id.as_str()
            ],
        )
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(SubmitSchemaBundleOutcome::Created(record))
    }

    async fn get_schema_deployment(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        digest: &str,
    ) -> Result<Option<SchemaDeploymentRecord>, SchemaDeploymentStoreError> {
        let connection = self.configured_connection().await.map_err(backend)?;
        let mut rows = connection
            .query(
                "SELECT record_json FROM schema_deployments
                 WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3 AND bundle_digest = ?4",
                params![tenant, SCOPE_KIND_TASK, scope.id.as_str(), digest],
            )
            .await
            .map_err(backend)?;
        let Some(row) = rows.next().await.map_err(backend)? else {
            return Ok(None);
        };
        let json: String = row.get(0).map_err(backend)?;
        decode(&json).map(Some)
    }

    async fn claim_schema_verification(
        &self,
        command: ClaimSchemaVerification,
    ) -> Result<ClaimSchemaVerificationOutcome, SchemaDeploymentStoreError> {
        validate_operation(
            &command.tenant,
            &command.scope,
            &command.bundle_digest,
            &command.operation,
        )?;
        if command.lease_expires_at <= command.logical_now {
            return Err(SchemaDeploymentStoreError::InvalidInput(
                "verification lease must end after logical now".into(),
            ));
        }
        let _permit = self
            .acquire_write_permit("schema_bundle_verify_claim", WritePriority::High)
            .await
            .map_err(backend)?;
        let connection = self.configured_connection().await.map_err(backend)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        if let Some((request_digest, bundle_digest, scope_id)) = load_idempotency(
            &tx,
            &command.tenant,
            "verify",
            &command.operation.idempotency_key,
        )
        .await?
        {
            if request_digest != command.operation.request_digest
                || bundle_digest != command.bundle_digest
                || scope_id != command.scope.id
            {
                return Err(SchemaDeploymentStoreError::IdempotencyConflict);
            }
            let record =
                load_deployment(&tx, &command.tenant, &command.scope, &command.bundle_digest)
                    .await?
                    .ok_or_else(|| {
                        backend("verification idempotency record lost its deployment")
                    })?;
            tx.commit().await.map_err(backend)?;
            let replay = record.verification_replay_record().unwrap_or(record);
            return Ok(ClaimSchemaVerificationOutcome::Replayed(replay));
        }
        let mut record =
            load_deployment(&tx, &command.tenant, &command.scope, &command.bundle_digest)
                .await?
                .ok_or(SchemaDeploymentStoreError::NotFound)?;
        let claimable = record.status == SchemaDeploymentStatus::Submitted
            || (record.status == SchemaDeploymentStatus::Verifying
                && record
                    .lease_expires_at
                    .is_some_and(|deadline| deadline <= command.logical_now));
        if !claimable {
            return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
        }
        record.status = SchemaDeploymentStatus::Verifying;
        record.verification_request_id = Some(command.operation.request_id.clone());
        record.fence = record
            .fence
            .checked_add(1)
            .ok_or_else(|| backend("fence exhausted"))?;
        record.committed_sequence = record
            .committed_sequence
            .checked_add(1)
            .ok_or_else(|| backend("sequence exhausted"))?;
        record.lease_expires_at = Some(command.lease_expires_at);
        write_deployment(&tx, &record).await?;
        insert_idempotency(
            &tx,
            &command.tenant,
            "verify",
            &command.operation,
            &command.scope,
            &command.bundle_digest,
        )
        .await?;
        tx.commit().await.map_err(backend)?;
        Ok(ClaimSchemaVerificationOutcome::Claimed(record))
    }

    async fn finish_schema_verification(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        digest: &str,
        expected_fence: u64,
        receipt: SchemaVerificationReceipt,
    ) -> Result<SchemaDeploymentRecord, SchemaDeploymentStoreError> {
        validate_digest("bundle digest", digest)?;
        validate_digest("verification input digest", &receipt.input_digest)?;
        let _permit = self
            .acquire_write_permit("schema_bundle_verify_finish", WritePriority::High)
            .await
            .map_err(backend)?;
        let connection = self.configured_connection().await.map_err(backend)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        let mut record = load_deployment(&tx, tenant, scope, digest)
            .await?
            .ok_or(SchemaDeploymentStoreError::NotFound)?;
        if record.status != SchemaDeploymentStatus::Verifying {
            return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
        }
        if record.fence != expected_fence {
            return Err(SchemaDeploymentStoreError::StaleFence);
        }
        let mut rows = tx
            .query(
                "SELECT receipt_json FROM schema_verification_receipts
                 WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3
                   AND bundle_digest = ?4 AND receipt_id = ?5",
                params![
                    tenant,
                    SCOPE_KIND_TASK,
                    scope.id.as_str(),
                    digest,
                    receipt.id.as_str()
                ],
            )
            .await
            .map_err(backend)?;
        if let Some(row) = rows.next().await.map_err(backend)? {
            let json: String = row.get(0).map_err(backend)?;
            let prior: SchemaVerificationReceipt = decode(&json)?;
            if prior != receipt {
                return Err(SchemaDeploymentStoreError::InvalidInput(
                    "verification receipt identity conflict".into(),
                ));
            }
        } else {
            drop(rows);
            tx.execute(
                "INSERT INTO schema_verification_receipts
                 (tenant, scope_kind, scope_id, bundle_digest, receipt_id, receipt_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    tenant,
                    SCOPE_KIND_TASK,
                    scope.id.as_str(),
                    digest,
                    receipt.id.as_str(),
                    encode(&receipt)?
                ],
            )
            .await
            .map_err(backend)?;
        }
        record.status = if receipt.passed {
            SchemaDeploymentStatus::Verified
        } else {
            SchemaDeploymentStatus::Rejected
        };
        record.lease_expires_at = None;
        record.verification_receipt_id = Some(receipt.id.clone());
        record.committed_sequence = record
            .committed_sequence
            .checked_add(1)
            .ok_or_else(|| backend("sequence exhausted"))?;
        record.verification_replay = Some(SchemaVerificationReplay {
            status: record.status,
            fence: record.fence,
            committed_sequence: record.committed_sequence,
            verification_receipt_id: receipt.id,
        });
        write_deployment(&tx, &record).await?;
        tx.commit().await.map_err(backend)?;
        Ok(record)
    }

    async fn activate_schema_bundle(
        &self,
        command: ActivateSchemaBundle,
    ) -> Result<ActivateSchemaBundleOutcome, SchemaDeploymentStoreError> {
        activation::activate(self, command).await
    }

    async fn retire_schema_bundle(
        &self,
        command: RetireSchemaBundle,
    ) -> Result<RetireSchemaBundleOutcome, SchemaDeploymentStoreError> {
        validate_operation(
            &command.tenant,
            &command.scope,
            &command.bundle_digest,
            &command.operation,
        )?;
        let tenant = command.tenant.as_str();
        let scope = &command.scope;
        let digest = command.bundle_digest.as_str();
        let _permit = self
            .acquire_write_permit("schema_bundle_retire", WritePriority::High)
            .await
            .map_err(backend)?;
        let connection = self.configured_connection().await.map_err(backend)?;
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(backend)?;
        if let Some((request_digest, bundle_digest, scope_id)) =
            load_idempotency(&tx, tenant, "retire", &command.operation.idempotency_key).await?
        {
            if request_digest != command.operation.request_digest
                || bundle_digest != command.bundle_digest
                || scope_id != command.scope.id
            {
                return Err(SchemaDeploymentStoreError::IdempotencyConflict);
            }
            let record = load_deployment(&tx, tenant, scope, digest)
                .await?
                .ok_or_else(|| backend("retirement idempotency record lost its deployment"))?;
            tx.commit().await.map_err(backend)?;
            return Ok(RetireSchemaBundleOutcome::Replayed(record));
        }
        let mut rows = tx
            .query(
                "SELECT pointer_json FROM schema_active_pointers
                 WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3",
                params![tenant, SCOPE_KIND_TASK, scope.id.as_str()],
            )
            .await
            .map_err(backend)?;
        let row = rows
            .next()
            .await
            .map_err(backend)?
            .ok_or(SchemaDeploymentStoreError::InvalidLifecycleTransition)?;
        let pointer: SchemaActivePointer = decode(&row.get::<String>(0).map_err(backend)?)?;
        drop(rows);
        if pointer.bundle_digest != digest {
            return Err(SchemaDeploymentStoreError::PredecessorMismatch);
        }
        if pointer.fence != command.expected_fence {
            return Err(SchemaDeploymentStoreError::StaleFence);
        }
        let mut record = load_deployment(&tx, tenant, scope, digest)
            .await?
            .ok_or(SchemaDeploymentStoreError::NotFound)?;
        if record.status != SchemaDeploymentStatus::Active || record.fence != command.expected_fence
        {
            return Err(SchemaDeploymentStoreError::StaleFence);
        }
        record.status = SchemaDeploymentStatus::Retired;
        record.retirement_request_id = Some(command.operation.request_id.clone());
        record.fence = record
            .fence
            .checked_add(1)
            .ok_or_else(|| backend("fence exhausted"))?;
        record.committed_sequence = record
            .committed_sequence
            .checked_add(1)
            .ok_or_else(|| backend("sequence exhausted"))?;
        write_deployment(&tx, &record).await?;
        tx.execute(
            "DELETE FROM schema_active_pointers
             WHERE tenant = ?1 AND scope_kind = ?2 AND scope_id = ?3",
            params![tenant, SCOPE_KIND_TASK, scope.id.as_str()],
        )
        .await
        .map_err(backend)?;
        insert_idempotency(&tx, tenant, "retire", &command.operation, scope, digest).await?;
        tx.commit().await.map_err(backend)?;
        Ok(RetireSchemaBundleOutcome::Retired(record))
    }

    async fn active_schema_pointer(
        &self,
        tenant: &str,
        scope: &SchemaScope,
    ) -> Result<Option<SchemaActivePointer>, SchemaDeploymentStoreError> {
        load_active_pointer(self, tenant, scope).await
    }

    impl_schema_migration_delegates!();
}
