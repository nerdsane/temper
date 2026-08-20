//! PostgreSQL transactions for ADR-0159 schema deployment lifecycle.

mod activation;
mod helpers;
mod migration;
#[macro_use]
mod migration_delegates;
#[cfg(test)]
mod tests;

use sqlx::{Acquire, Postgres, Row, Transaction};
use temper_runtime::persistence::schema_deployment::{
    ActivateSchemaBundle, ActivateSchemaBundleOutcome, ClaimSchemaVerification,
    ClaimSchemaVerificationOutcome, CommitSchemaMigrationBatch, CreateSchemaMigration,
    CreateSchemaMigrationOutcome, ReserveSchemaMigrationRetry, RetireSchemaBundle,
    RetireSchemaBundleOutcome, SchemaActivePointer, SchemaDeploymentRecord, SchemaDeploymentStatus,
    SchemaDeploymentStore, SchemaDeploymentStoreError, SchemaMigrationJob,
    SchemaMigrationRetryReservation, SchemaMigrationShadowRow, SchemaMigrationValidationReceipt,
    SchemaOperationIdentity, SchemaScope, SchemaVerificationReceipt, SchemaVerificationReplay,
    SubmitSchemaBundle, SubmitSchemaBundleOutcome,
};

use crate::PostgresEventStore;
use helpers::*;

const SCOPE_KIND_TASK: &str = "task";

impl SchemaDeploymentStore for PostgresEventStore {
    async fn submit_schema_bundle(
        &self,
        command: SubmitSchemaBundle,
    ) -> Result<SubmitSchemaBundleOutcome, SchemaDeploymentStoreError> {
        validate_submit(&command)?;
        let mut connection = self.pool().acquire().await.map_err(backend)?;
        let mut tx = connection.begin().await.map_err(backend)?;
        lock_schema_key(
            &mut tx,
            "idempotency",
            &[&command.bundle.tenant, "submit", &command.idempotency_key],
        )
        .await?;
        let idem = sqlx::query(
            "SELECT request_digest, bundle_digest, scope_id
             FROM schema_deployment_idempotency
             WHERE tenant = $1 AND operation = 'submit' AND idempotency_key = $2
             FOR UPDATE",
        )
        .bind(&command.bundle.tenant)
        .bind(&command.idempotency_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;
        if let Some(row) = idem {
            let request_digest: String = row.get("request_digest");
            if request_digest != command.request_digest {
                return Err(SchemaDeploymentStoreError::IdempotencyConflict);
            }
            let digest: String = row.get("bundle_digest");
            let scope = SchemaScope {
                kind: command.bundle.scope.kind.clone(),
                id: row.get("scope_id"),
            };
            let record = locked_deployment(&mut tx, &command.bundle.tenant, &scope, &digest)
                .await?
                .ok_or_else(|| backend("idempotency record lost its deployment"))?;
            tx.commit().await.map_err(backend)?;
            return Ok(SubmitSchemaBundleOutcome::Replayed(record));
        }

        if let Some(existing) = locked_deployment(
            &mut tx,
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
            sqlx::query(
                "INSERT INTO schema_deployment_idempotency
                 (tenant, operation, idempotency_key, request_digest, bundle_digest, scope_kind, scope_id)
                 VALUES ($1, 'submit', $2, $3, $4, $5, $6)",
            )
            .bind(&command.bundle.tenant)
            .bind(&command.idempotency_key)
            .bind(&command.request_digest)
            .bind(&command.bundle.digest)
            .bind(SCOPE_KIND_TASK)
            .bind(&command.bundle.scope.id)
            .execute(&mut *tx)
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
        sqlx::query(
            "INSERT INTO schema_deployments
             (tenant, scope_kind, scope_id, bundle_digest, record_json)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&command.bundle.tenant)
        .bind(SCOPE_KIND_TASK)
        .bind(&command.bundle.scope.id)
        .bind(&command.bundle.digest)
        .bind(encode(&record)?)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        sqlx::query(
            "INSERT INTO schema_deployment_idempotency
             (tenant, operation, idempotency_key, request_digest, bundle_digest, scope_kind, scope_id)
             VALUES ($1, 'submit', $2, $3, $4, $5, $6)",
        )
        .bind(&command.bundle.tenant)
        .bind(&command.idempotency_key)
        .bind(&command.request_digest)
        .bind(&command.bundle.digest)
        .bind(SCOPE_KIND_TASK)
        .bind(&command.bundle.scope.id)
        .execute(&mut *tx)
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
        let row = sqlx::query(
            "SELECT record_json FROM schema_deployments
             WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3 AND bundle_digest = $4",
        )
        .bind(tenant)
        .bind(SCOPE_KIND_TASK)
        .bind(&scope.id)
        .bind(digest)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        row.map(|row| decode(row.get("record_json"))).transpose()
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
        let mut connection = self.pool().acquire().await.map_err(backend)?;
        let mut tx = connection.begin().await.map_err(backend)?;
        lock_schema_key(
            &mut tx,
            "idempotency",
            &[
                &command.tenant,
                "verify",
                &command.operation.idempotency_key,
            ],
        )
        .await?;
        if let Some((request_digest, bundle_digest, scope_id)) = locked_idempotency(
            &mut tx,
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
            let record = locked_deployment(
                &mut tx,
                &command.tenant,
                &command.scope,
                &command.bundle_digest,
            )
            .await?
            .ok_or_else(|| backend("verification idempotency record lost its deployment"))?;
            tx.commit().await.map_err(backend)?;
            let replay = record.verification_replay_record().unwrap_or(record);
            return Ok(ClaimSchemaVerificationOutcome::Replayed(replay));
        }
        let mut record = locked_deployment(
            &mut tx,
            &command.tenant,
            &command.scope,
            &command.bundle_digest,
        )
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
        write_deployment(&mut tx, &record).await?;
        insert_idempotency(
            &mut tx,
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
        let mut connection = self.pool().acquire().await.map_err(backend)?;
        let mut tx = connection.begin().await.map_err(backend)?;
        let mut record = locked_deployment(&mut tx, tenant, scope, digest)
            .await?
            .ok_or(SchemaDeploymentStoreError::NotFound)?;
        if record.status != SchemaDeploymentStatus::Verifying {
            return Err(SchemaDeploymentStoreError::InvalidLifecycleTransition);
        }
        if record.fence != expected_fence {
            return Err(SchemaDeploymentStoreError::StaleFence);
        }
        let existing = sqlx::query(
            "SELECT receipt_json FROM schema_verification_receipts
             WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3
               AND bundle_digest = $4 AND receipt_id = $5 FOR UPDATE",
        )
        .bind(tenant)
        .bind(SCOPE_KIND_TASK)
        .bind(&scope.id)
        .bind(digest)
        .bind(&receipt.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;
        if let Some(row) = existing {
            let prior: SchemaVerificationReceipt = decode(row.get("receipt_json"))?;
            if prior != receipt {
                return Err(SchemaDeploymentStoreError::InvalidInput(
                    "verification receipt identity conflict".into(),
                ));
            }
        } else {
            sqlx::query(
                "INSERT INTO schema_verification_receipts
                 (tenant, scope_kind, scope_id, bundle_digest, receipt_id, receipt_json)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(tenant)
            .bind(SCOPE_KIND_TASK)
            .bind(&scope.id)
            .bind(digest)
            .bind(&receipt.id)
            .bind(encode(&receipt)?)
            .execute(&mut *tx)
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
        write_deployment(&mut tx, &record).await?;
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
        let mut connection = self.pool().acquire().await.map_err(backend)?;
        let mut tx = connection.begin().await.map_err(backend)?;
        lock_schema_key(
            &mut tx,
            "idempotency",
            &[tenant, "retire", &command.operation.idempotency_key],
        )
        .await?;
        if let Some((request_digest, bundle_digest, scope_id)) = locked_idempotency(
            &mut tx,
            tenant,
            "retire",
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
            let record = locked_deployment(&mut tx, tenant, scope, digest)
                .await?
                .ok_or_else(|| backend("retirement idempotency record lost its deployment"))?;
            tx.commit().await.map_err(backend)?;
            return Ok(RetireSchemaBundleOutcome::Replayed(record));
        }
        lock_schema_key(&mut tx, "scope", &[tenant, SCOPE_KIND_TASK, &scope.id]).await?;
        let row = sqlx::query(
            "SELECT pointer_json FROM schema_active_pointers
             WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3 FOR UPDATE",
        )
        .bind(tenant)
        .bind(SCOPE_KIND_TASK)
        .bind(&scope.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?
        .ok_or(SchemaDeploymentStoreError::InvalidLifecycleTransition)?;
        let pointer: SchemaActivePointer = decode(row.get("pointer_json"))?;
        if pointer.bundle_digest != digest {
            return Err(SchemaDeploymentStoreError::PredecessorMismatch);
        }
        if pointer.fence != command.expected_fence {
            return Err(SchemaDeploymentStoreError::StaleFence);
        }
        let mut record = locked_deployment(&mut tx, tenant, scope, digest)
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
        write_deployment(&mut tx, &record).await?;
        sqlx::query(
            "DELETE FROM schema_active_pointers
             WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3",
        )
        .bind(tenant)
        .bind(SCOPE_KIND_TASK)
        .bind(&scope.id)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        insert_idempotency(&mut tx, tenant, "retire", &command.operation, scope, digest).await?;
        tx.commit().await.map_err(backend)?;
        Ok(RetireSchemaBundleOutcome::Retired(record))
    }

    async fn active_schema_pointer(
        &self,
        tenant: &str,
        scope: &SchemaScope,
    ) -> Result<Option<SchemaActivePointer>, SchemaDeploymentStoreError> {
        let row = sqlx::query(
            "SELECT pointer_json FROM schema_active_pointers
             WHERE tenant = $1 AND scope_kind = $2 AND scope_id = $3",
        )
        .bind(tenant)
        .bind(SCOPE_KIND_TASK)
        .bind(&scope.id)
        .fetch_optional(self.pool())
        .await
        .map_err(backend)?;
        row.map(|row| decode(row.get("pointer_json"))).transpose()
    }

    impl_schema_migration_delegates!();
}
