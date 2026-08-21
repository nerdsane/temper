//! Shared schema-deployment semantics used by HTTP and typed WASM adapters.

mod completion;
mod error;
mod http;
mod lifecycle;
mod migration;
mod registry;
mod runner;
mod supervisor;
mod support;
#[cfg(test)]
mod support_test;
mod validation;

use error::ServiceError;

pub(crate) use http::*;
use support::*;

use std::collections::BTreeMap;

use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::persistence::schema_deployment::{
    ActivateSchemaBundle, ActivateSchemaBundleOutcome, ClaimSchemaVerification,
    ClaimSchemaVerificationOutcome, CommitSchemaMigrationBatch, CreateSchemaMigration,
    CreateSchemaMigrationOutcome, ReserveSchemaMigrationRetry, RetireSchemaBundle,
    RetireSchemaBundleOutcome, SchemaBundleRecord, SchemaDeploymentRecord, SchemaDeploymentStatus,
    SchemaDeploymentStoreError, SchemaExecutionPin, SchemaMigrationBatchReceipt,
    SchemaMigrationBudgets, SchemaMigrationJob, SchemaMigrationShadowRow, SchemaMigrationStatus,
    SchemaMigrationValidationReceipt, SchemaOperationIdentity, SchemaScope, SchemaScopeKind,
    SchemaVerificationReceipt, SubmitSchemaBundle, SubmitSchemaBundleOutcome,
};
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_spec::{
    IoaSourceInput, MigrationArtifactInput, PolicyArtifactInput, ScopedBundleBudgets,
    ScopedSpecBundle, ScopedSpecBundleInput, WasmArtifactInput,
};
use temper_wasm_sdk::schema_deployment::{
    ActivateSchemaBundleRequestV1, GetSchemaBundleRequestV1, GetSchemaMigrationRequestV1,
    RetireSchemaBundleRequestV1, RetrySchemaMigrationRequestV1, SchemaBundleBudgetsV1,
    SchemaDeploymentErrorV1, SchemaDeploymentReceiptV1, SchemaDeploymentResponseV1,
    SchemaMigrationBudgetsV1, SchemaMigrationInputV1, SchemaMigrationLogicalContextV1,
    SchemaMigrationOutputV1, SchemaMigrationReceiptV1, SchemaScopeV1,
    StartSchemaMigrationRequestV1, SubmitSchemaBundleRequestV1, VerifySchemaBundleRequestV1,
};

use crate::authz::{DenialInput, record_authz_denial, require_authenticated_context};
use crate::entity_actor::EntityEvent;
use crate::state::ServerState;

const BUNDLE_CONTRACT: &str = temper_spec::bundle::SCOPED_SPEC_BUNDLE_CONTRACT_V1;

/// Shared governed service; adapters supply host-resolved tenant and principal.
pub(crate) struct GovernedSchemaDeploymentService<'a> {
    state: &'a ServerState,
}

impl<'a> GovernedSchemaDeploymentService<'a> {
    pub(crate) fn new(state: &'a ServerState) -> Self {
        Self { state }
    }

    fn store(
        &self,
    ) -> Result<&std::sync::Arc<dyn crate::storage::SchemaDeploymentStoreDyn>, ServiceError> {
        self.state
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.schema_deployments.as_ref())
            .ok_or_else(|| {
                ServiceError::new(
                    "backend_unavailable",
                    "schema deployment store is unavailable",
                    true,
                )
            })
    }

    async fn authorize(
        &self,
        tenant: &str,
        security: &SecurityContext,
        action: &str,
        scope: &SchemaScope,
        digest: Option<&str>,
    ) -> Result<(), ServiceError> {
        let mut attributes = BTreeMap::from([
            (
                "scope_kind".into(),
                serde_json::Value::String("task".into()),
            ),
            (
                "scope_id".into(),
                serde_json::Value::String(scope.id.clone()),
            ),
        ]);
        if let Some(digest) = digest {
            attributes.insert("bundle_digest".into(), digest.into());
        }
        if let Err(denial) = self.state.authorize_with_context(
            security,
            action,
            "SchemaDeployment",
            &attributes,
            tenant,
        ) {
            let pending = record_authz_denial(
                self.state,
                DenialInput {
                    tenant,
                    security_ctx: security,
                    agent_id_override: None,
                    action,
                    resource_type: "SchemaDeployment",
                    resource_id: &scope.id,
                    resource_attrs: serde_json::Value::Object(attributes.into_iter().collect()),
                    reason: &denial.to_string(),
                    module_name: None,
                    from_status: None,
                    intent: None,
                    session_id: None,
                    spec_governed: None,
                },
            )
            .await;
            return Err(ServiceError::authorization(pending.id));
        }
        Ok(())
    }

    pub(crate) async fn submit(
        &self,
        tenant: &str,
        security: &SecurityContext,
        request: SubmitSchemaBundleRequestV1,
    ) -> Result<SchemaDeploymentReceiptV1, ServiceError> {
        let scope = parse_scope(&request.scope)?;
        self.authorize(
            tenant,
            security,
            "schema_bundle_submit",
            &scope,
            Some(&request.expected_digest),
        )
        .await?;
        if request.canonicalization_version != BUNDLE_CONTRACT {
            return Err(ServiceError::new(
                "invalid_bundle",
                "unsupported canonicalization version",
                false,
            ));
        }
        let compiled = ScopedSpecBundle::compile(ScopedSpecBundleInput {
            scope_id: scope.id.clone(),
            predecessor_digest: request.expected_predecessor.clone(),
            csdl_xml: request.csdl,
            ioa_sources: request
                .ioa
                .into_iter()
                .map(|source| IoaSourceInput {
                    entity_type: source.entity_type,
                    source: source.source,
                })
                .collect(),
            cedar_policies: request
                .cedar_policies
                .into_iter()
                .map(|policy| PolicyArtifactInput {
                    name: policy.name,
                    source: policy.source,
                })
                .collect(),
            wasm_modules: request
                .wasm_modules
                .into_iter()
                .map(|module| WasmArtifactInput {
                    name: module.name,
                    artifact_digest: module.artifact_digest,
                })
                .collect(),
            migration: request.migration.map(|migration| MigrationArtifactInput {
                name: migration.name,
                artifact_digest: migration.artifact_digest,
                abi_version: migration.abi_version,
            }),
            budgets: to_spec_budgets(&request.budgets),
        })
        .map_err(|error| ServiceError::new("invalid_bundle", error.to_string(), false))?;
        if compiled.digest() != request.expected_digest {
            return Err(ServiceError::new(
                "digest_mismatch",
                "computed bundle digest differs from expected_digest",
                false,
            ));
        }
        let bundle = SchemaBundleRecord {
            tenant: tenant.to_string(),
            scope: scope.clone(),
            digest: compiled.digest().to_string(),
            predecessor_digest: compiled.predecessor_digest().map(str::to_string),
            canonical_csdl: compiled.canonical_csdl().to_string(),
            canonical_ioa: compiled
                .ioa_specs()
                .iter()
                .map(|spec| (spec.entity_type.clone(), spec.canonical_source.clone()))
                .collect(),
            cedar_policies: compiled
                .cedar_policies()
                .iter()
                .map(|policy| (policy.name.clone(), policy.source.clone()))
                .collect(),
            wasm_module_digests: compiled
                .wasm_modules()
                .iter()
                .map(|module| (module.name.clone(), module.artifact_digest.clone()))
                .collect(),
            migration_module_name: compiled.migration().map(|migration| migration.name.clone()),
            migration_module_digest: compiled
                .migration()
                .map(|migration| migration.artifact_digest.clone()),
            migration_abi_version: compiled
                .migration()
                .map(|migration| migration.abi_version.clone()),
            canonical_budgets: serde_json::to_string(&request.budgets)
                .map_err(|error| ServiceError::new("invalid_bundle", error.to_string(), false))?,
        };
        let request_digest = canonical_request_digest(&bundle)?;
        let outcome = self
            .store()?
            .submit_schema_bundle(SubmitSchemaBundle {
                bundle,
                idempotency_key: request.idempotency_key,
                request_digest,
                request_id: request.request_id,
            })
            .await
            .map_err(ServiceError::from_store)?;
        let (record, created) = match outcome {
            SubmitSchemaBundleOutcome::Created(record) => (record, true),
            SubmitSchemaBundleOutcome::Replayed(record) => (record, false),
        };
        self.stage_registry_bundle(&record)?;
        if created {
            emit_schema_lifecycle(
                tenant,
                "SchemaDeployment",
                &record.bundle.digest,
                "submit",
                "absent",
                "submitted",
                &record.bundle.scope,
            );
        }
        Ok(receipt(&record))
    }

    pub(crate) async fn get(
        &self,
        tenant: &str,
        security: &SecurityContext,
        scope: SchemaScope,
        digest: &str,
    ) -> Result<SchemaDeploymentReceiptV1, ServiceError> {
        self.authorize(tenant, security, "schema_bundle_get", &scope, Some(digest))
            .await?;
        let record = self
            .store()?
            .get_schema_deployment(tenant, &scope, digest)
            .await
            .map_err(ServiceError::from_store)?
            .ok_or_else(|| {
                ServiceError::new("invalid_bundle", "schema deployment was not found", false)
            })?;
        Ok(receipt(&record))
    }

    pub(crate) async fn get_request(
        &self,
        tenant: &str,
        security: &SecurityContext,
        request: GetSchemaBundleRequestV1,
    ) -> Result<SchemaDeploymentReceiptV1, ServiceError> {
        let scope = parse_scope(&request.scope)?;
        let mut result = self
            .get(tenant, security, scope, &request.bundle_digest)
            .await?;
        result.request_id = request.request_id;
        Ok(result)
    }
}
