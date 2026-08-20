//! Typed v1 schema-deployment transport contracts shared by HTTP and WASM.

use serde::{Deserialize, Serialize};

/// Closed ABI identifier for schema-deployment host calls.
pub const SCHEMA_DEPLOYMENT_ABI_V1: &str = "temper-schema-deployment/v1";

/// One task-local scope supplied without tenant or principal authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaScopeV1 {
    /// Closed v1 kind; must be `task`.
    pub kind: String,
    /// Opaque non-empty task identity.
    pub id: String,
}

/// One fully-qualified IOA source in a submit request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaIoaSourceV1 {
    /// Fully-qualified CSDL type name.
    pub entity_type: String,
    /// Typed IOA TOML source.
    pub source: String,
}

/// One Cedar artifact bound into bundle identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaPolicyArtifactV1 {
    pub name: String,
    pub source: String,
}

/// One immutable WASM module binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaWasmArtifactV1 {
    pub name: String,
    pub artifact_digest: String,
}

/// Optional closed migration module binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaMigrationArtifactV1 {
    pub name: String,
    pub artifact_digest: String,
    pub abi_version: String,
}

/// Positive execution budgets included in immutable identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaBundleBudgetsV1 {
    pub verification_steps: u64,
    pub migration_fuel_per_entity: u64,
    pub migration_memory_pages: u32,
    pub migration_input_bytes: u32,
    pub migration_output_bytes: u32,
    pub migration_entities_per_batch: u32,
    pub migration_total_entities: u64,
    pub migration_total_batches: u64,
    pub migration_attempts: u32,
}

/// Idempotent immutable bundle submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitSchemaBundleRequestV1 {
    pub request_id: String,
    pub idempotency_key: String,
    pub scope: SchemaScopeV1,
    pub expected_predecessor: Option<String>,
    pub expected_digest: String,
    pub canonicalization_version: String,
    pub csdl: String,
    pub ioa: Vec<SchemaIoaSourceV1>,
    #[serde(default)]
    pub cedar_policies: Vec<SchemaPolicyArtifactV1>,
    #[serde(default)]
    pub wasm_modules: Vec<SchemaWasmArtifactV1>,
    pub migration: Option<SchemaMigrationArtifactV1>,
    pub budgets: SchemaBundleBudgetsV1,
}

/// Read one immutable bundle lifecycle receipt without artifact contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetSchemaBundleRequestV1 {
    pub request_id: String,
    pub scope: SchemaScopeV1,
    pub bundle_digest: String,
}

/// Claim and execute verification for an immutable submitted digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifySchemaBundleRequestV1 {
    pub request_id: String,
    pub idempotency_key: String,
    pub scope: SchemaScopeV1,
    pub bundle_digest: String,
}

/// Atomically activate a verified immutable bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateSchemaBundleRequestV1 {
    pub request_id: String,
    pub idempotency_key: String,
    pub scope: SchemaScopeV1,
    pub bundle_digest: String,
    pub expected_predecessor: Option<String>,
    pub expected_fence: u64,
    pub verification_receipt_id: String,
}

/// Atomically retire the current active bundle while preserving pinned reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetireSchemaBundleRequestV1 {
    pub request_id: String,
    pub idempotency_key: String,
    pub scope: SchemaScopeV1,
    pub bundle_digest: String,
    pub expected_fence: u64,
}

/// Deterministic position supplied to one pure migration invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaMigrationLogicalContextV1 {
    /// Stable migration batch identity.
    pub batch_id: String,
    /// Zero-based position inside the batch.
    pub item_index: u32,
}

/// Canonical input passed to `temper_schema_migrate_v1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaMigrationInputV1 {
    /// Closed ABI version. V1 requires the value `1`.
    pub abi_version: u32,
    /// Immutable source bundle digest.
    pub source_bundle_digest: String,
    /// Immutable target bundle digest.
    pub target_bundle_digest: String,
    /// Fully-qualified entity type being transformed.
    pub entity_type: String,
    /// Stable tenant-local entity identity.
    pub entity_id: String,
    /// Source event sequence used for replay validation.
    pub source_sequence: u64,
    /// Canonical JSON object encoded as a string.
    pub canonical_state_json: String,
    /// Deterministic batch-local position.
    pub logical_context: SchemaMigrationLogicalContextV1,
}

/// Closed result returned by `temper_schema_migrate_v1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SchemaMigrationOutputV1 {
    /// Preserve the canonical source state byte-for-byte.
    Unchanged,
    /// Replace the source state with a canonical JSON object.
    Replace { canonical_state_json: String },
    /// Reject this entity with a stable bounded diagnostic.
    Reject { code: String, message: String },
}

/// Positive end-to-end budgets for one durable migration job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaMigrationBudgetsV1 {
    pub fuel_per_entity: u64,
    pub memory_pages: u32,
    pub input_bytes: u32,
    pub output_bytes: u32,
    pub entities_per_batch: u32,
    pub total_entities: u64,
    pub total_batches: u64,
    pub attempts: u32,
}

/// Idempotently start a fenced shadow migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartSchemaMigrationRequestV1 {
    pub request_id: String,
    pub idempotency_key: String,
    pub scope: SchemaScopeV1,
    pub source_bundle_digest: String,
    pub target_bundle_digest: String,
    pub verification_receipt_id: String,
    pub expected_fence: u64,
    pub budgets: SchemaMigrationBudgetsV1,
}

/// Read one durable migration without exposing entity payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetSchemaMigrationRequestV1 {
    pub request_id: String,
    pub scope: SchemaScopeV1,
    pub job_id: String,
}

/// Retry one expired or submitted bounded migration batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrySchemaMigrationRequestV1 {
    pub request_id: String,
    pub idempotency_key: String,
    pub scope: SchemaScopeV1,
    pub job_id: String,
}

/// Redacted migration receipt shared by HTTP and typed WASM adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaMigrationReceiptV1 {
    pub request_id: String,
    pub job_id: String,
    pub scope: SchemaScopeV1,
    pub source_bundle_digest: String,
    pub target_bundle_digest: String,
    pub status: String,
    pub fence: u64,
    pub scan_cursor: Option<(String, String)>,
    pub consumed_entities: u64,
    pub consumed_batches: u64,
    pub validation_receipt_id: Option<String>,
    pub migration_receipt_id: Option<String>,
    pub committed_sequence: u64,
}

/// Redacted deployment receipt returned by every adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaDeploymentReceiptV1 {
    pub request_id: String,
    pub scope: SchemaScopeV1,
    pub bundle_digest: String,
    pub predecessor: Option<String>,
    pub status: String,
    pub fence: u64,
    pub verification_receipt_id: Option<String>,
    pub migration_receipt_id: Option<String>,
    pub committed_sequence: u64,
}

/// Stable adapter-neutral schema-deployment failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaDeploymentErrorV1 {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    /// Pending Cedar decision that a human may resolve, when authorization denied.
    pub decision_id: Option<String>,
}

/// Closed host-call operation union.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "request", rename_all = "snake_case")]
pub enum SchemaDeploymentOperationV1 {
    Submit(SubmitSchemaBundleRequestV1),
    GetBundle(GetSchemaBundleRequestV1),
    Verify(VerifySchemaBundleRequestV1),
    Activate(ActivateSchemaBundleRequestV1),
    Retire(RetireSchemaBundleRequestV1),
    StartMigration(StartSchemaMigrationRequestV1),
    GetMigration(GetSchemaMigrationRequestV1),
    RetryMigration(RetrySchemaMigrationRequestV1),
}

/// Encoded typed WASM request; tenant and principal are intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaDeploymentRequestV1 {
    pub abi: String,
    pub operation: SchemaDeploymentOperationV1,
}

/// Encoded response shared by HTTP and WASM adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SchemaDeploymentResponseV1 {
    Ok { receipt: SchemaDeploymentReceiptV1 },
    Migration { receipt: SchemaMigrationReceiptV1 },
    Error { error: SchemaDeploymentErrorV1 },
}

/// Typed guest client over the invocation-bound schema-deployment host call.
#[derive(Debug, Default, Clone, Copy)]
pub struct SchemaDeploymentClient;

impl SchemaDeploymentClient {
    /// Submit one immutable task-scoped bundle.
    pub fn submit(
        &self,
        request: SubmitSchemaBundleRequestV1,
    ) -> Result<SchemaDeploymentReceiptV1, SchemaDeploymentErrorV1> {
        call(SchemaDeploymentOperationV1::Submit(request))
    }

    /// Read one redacted immutable bundle lifecycle receipt.
    pub fn get_bundle(
        &self,
        request: GetSchemaBundleRequestV1,
    ) -> Result<SchemaDeploymentReceiptV1, SchemaDeploymentErrorV1> {
        call(SchemaDeploymentOperationV1::GetBundle(request))
    }

    /// Run the governed verification cascade.
    pub fn verify(
        &self,
        request: VerifySchemaBundleRequestV1,
    ) -> Result<SchemaDeploymentReceiptV1, SchemaDeploymentErrorV1> {
        call(SchemaDeploymentOperationV1::Verify(request))
    }

    /// Atomically activate one verified bundle.
    pub fn activate(
        &self,
        request: ActivateSchemaBundleRequestV1,
    ) -> Result<SchemaDeploymentReceiptV1, SchemaDeploymentErrorV1> {
        call(SchemaDeploymentOperationV1::Activate(request))
    }

    /// Retire the current active bundle without deleting immutable artifacts.
    pub fn retire(
        &self,
        request: RetireSchemaBundleRequestV1,
    ) -> Result<SchemaDeploymentReceiptV1, SchemaDeploymentErrorV1> {
        call(SchemaDeploymentOperationV1::Retire(request))
    }

    /// Start one durable bounded shadow migration.
    pub fn start_migration(
        &self,
        request: StartSchemaMigrationRequestV1,
    ) -> Result<SchemaMigrationReceiptV1, SchemaDeploymentErrorV1> {
        call_migration(SchemaDeploymentOperationV1::StartMigration(request))
    }

    /// Read one redacted durable migration receipt.
    pub fn get_migration(
        &self,
        request: GetSchemaMigrationRequestV1,
    ) -> Result<SchemaMigrationReceiptV1, SchemaDeploymentErrorV1> {
        call_migration(SchemaDeploymentOperationV1::GetMigration(request))
    }

    /// Retry one submitted or expired migration worker claim.
    pub fn retry_migration(
        &self,
        request: RetrySchemaMigrationRequestV1,
    ) -> Result<SchemaMigrationReceiptV1, SchemaDeploymentErrorV1> {
        call_migration(SchemaDeploymentOperationV1::RetryMigration(request))
    }
}

fn call(
    operation: SchemaDeploymentOperationV1,
) -> Result<SchemaDeploymentReceiptV1, SchemaDeploymentErrorV1> {
    let request = SchemaDeploymentRequestV1 {
        abi: SCHEMA_DEPLOYMENT_ABI_V1.into(),
        operation,
    };
    let bytes = serde_json::to_vec(&request).map_err(|error| {
        local_error(
            "invalid_bundle",
            format!("failed to encode schema deployment request: {error}"),
        )
    })?;
    let response = call_host(&bytes)?;
    match response {
        SchemaDeploymentResponseV1::Ok { receipt } => Ok(receipt),
        SchemaDeploymentResponseV1::Migration { .. } => Err(local_error(
            "backend_unavailable",
            "schema deployment host returned a migration receipt".into(),
        )),
        SchemaDeploymentResponseV1::Error { error } => Err(error),
    }
}

fn call_migration(
    operation: SchemaDeploymentOperationV1,
) -> Result<SchemaMigrationReceiptV1, SchemaDeploymentErrorV1> {
    let request = SchemaDeploymentRequestV1 {
        abi: SCHEMA_DEPLOYMENT_ABI_V1.into(),
        operation,
    };
    let bytes = serde_json::to_vec(&request).map_err(|error| {
        local_error(
            "invalid_bundle",
            format!("failed to encode schema migration request: {error}"),
        )
    })?;
    match call_host(&bytes)? {
        SchemaDeploymentResponseV1::Migration { receipt } => Ok(receipt),
        SchemaDeploymentResponseV1::Ok { .. } => Err(local_error(
            "backend_unavailable",
            "schema deployment host returned a bundle receipt".into(),
        )),
        SchemaDeploymentResponseV1::Error { error } => Err(error),
    }
}

fn local_error(code: &str, message: String) -> SchemaDeploymentErrorV1 {
    SchemaDeploymentErrorV1 {
        code: code.into(),
        message,
        retryable: false,
        decision_id: None,
    }
}

#[cfg(target_arch = "wasm32")]
fn call_host(bytes: &[u8]) -> Result<SchemaDeploymentResponseV1, SchemaDeploymentErrorV1> {
    let handle =
        unsafe { crate::host::host_temper_data_call(bytes.as_ptr() as i32, bytes.len() as i32) };
    if handle <= 0 || handle > i32::MAX as i64 {
        return Err(local_error(
            "backend_unavailable",
            format!("schema deployment host returned ABI code {handle}"),
        ));
    }
    let handle = handle as i32;
    let len = unsafe { crate::host::host_temper_data_response_len(handle) };
    if len < 0 {
        return Err(local_error(
            "backend_unavailable",
            "schema deployment host returned an invalid response handle".into(),
        ));
    }
    let mut response = vec![0u8; len as usize];
    let read = unsafe {
        crate::host::host_temper_data_response_read(handle, 0, response.as_mut_ptr() as i32, len)
    };
    let close = unsafe { crate::host::host_temper_data_response_close(handle) };
    if read != len || close != 0 {
        return Err(local_error(
            "backend_unavailable",
            "failed to read or close schema deployment response".into(),
        ));
    }
    serde_json::from_slice(&response).map_err(|error| {
        local_error(
            "backend_unavailable",
            format!("failed to decode schema deployment response: {error}"),
        )
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn call_host(_bytes: &[u8]) -> Result<SchemaDeploymentResponseV1, SchemaDeploymentErrorV1> {
    Err(local_error(
        "backend_unavailable",
        "schema deployment host is only available on wasm32".into(),
    ))
}
