//! Governed stream-descriptor migration contracts.

use serde::{Deserialize, Serialize};

use super::SchemaScopeV1;

/// Exact schema or installed-application target whose stream contract is migrated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StreamDescriptorMigrationTargetV1 {
    /// One immutable task-scoped schema bundle.
    TaskBundle {
        /// Tenant-local task scope.
        scope: SchemaScopeV1,
        /// Immutable canonical bundle digest.
        bundle_digest: String,
    },
    /// One immutable installed-application model closure.
    InstalledApplication {
        /// Stable application identity.
        application_id: String,
        /// Canonical semantic digest of the application model closure.
        semantic_digest: String,
    },
}

/// Positive bounds consumed by each inventory page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamDescriptorMigrationBudgetsV1 {
    /// Maximum subjects examined by one page.
    pub max_subjects: u32,
    /// Maximum historical events examined per subject.
    pub max_events_per_subject: u32,
    /// Maximum blob bytes read and hashed by one page.
    pub max_blob_bytes: u64,
}

/// Idempotently create a durable migration job for an exact target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartStreamDescriptorMigrationRequestV1 {
    /// Transport correlation identity.
    pub request_id: String,
    /// Stable idempotency identity for this target creation.
    pub idempotency_key: String,
    /// Exact immutable deployment target.
    pub target: StreamDescriptorMigrationTargetV1,
    /// Expected canonical digest of the verified stream capability set.
    pub expected_capability_digest: String,
    /// Required descriptor contract version; version one is currently supported.
    pub descriptor_contract_version: u16,
    /// Positive per-page work budgets.
    pub budgets: StreamDescriptorMigrationBudgetsV1,
}

/// Advance one durable job by at most one bounded platform-owned page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvanceStreamDescriptorMigrationRequestV1 {
    /// Transport correlation identity.
    pub request_id: String,
    /// Stable idempotency identity for this page advance.
    pub idempotency_key: String,
    /// Platform-minted durable migration job identity.
    pub job_id: String,
}

/// Read one durable migration job without exposing content identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetStreamDescriptorMigrationRequestV1 {
    /// Transport correlation identity.
    pub request_id: String,
    /// Platform-minted durable migration job identity.
    pub job_id: String,
}

/// Read a bounded page of unresolved classifications for operator repair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListUnresolvedStreamDescriptorsRequestV1 {
    /// Transport correlation identity.
    pub request_id: String,
    /// Platform-minted durable migration job identity.
    pub job_id: String,
    /// Opaque cursor returned by the previous unresolved page.
    pub after: Option<String>,
    /// Positive bounded number of redacted entries to return.
    pub limit: u32,
}

/// Redacted progress and exact activation evidence for a durable migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamDescriptorMigrationReceiptV1 {
    /// Original transport correlation identity for this exact receipt.
    pub request_id: String,
    /// Platform-minted durable migration job identity.
    pub job_id: String,
    /// Exact immutable deployment target.
    pub target: StreamDescriptorMigrationTargetV1,
    /// Canonical digest of the verified stream capability set.
    pub capability_digest: String,
    /// Descriptor contract version bound to this evidence.
    pub descriptor_contract_version: u16,
    /// Closed progress classification: migrating, unresolved, or completed.
    pub status: String,
    /// Opaque platform-owned cursor. Callers may persist but cannot manufacture it.
    pub cursor: Option<String>,
    /// Cumulative number of subjects examined, including stable rescans.
    pub scanned_subjects: u64,
    /// Number of distinct subjects proven descriptor-complete.
    pub migrated_subjects: u64,
    /// Current number of unresolved subjects.
    pub unresolved_subjects: u64,
    /// Redacted outcomes from the exact bounded page committed by this receipt.
    pub page_outcomes: Vec<StreamDescriptorMigrationPageOutcomeV1>,
    /// Present only for a terminal, zero-unresolved inventory at the current fence.
    /// Durable completion evidence accepted by the matching activation target.
    pub completion_receipt_id: Option<String>,
    /// Durable job sequence at which this exact receipt was committed.
    pub committed_sequence: u64,
}

/// Redacted durable result for one subject in a committed migration page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamDescriptorMigrationPageOutcomeV1 {
    /// Opaque stable digest of the subject identity.
    pub subject_digest: String,
    /// Bounded classification such as `migrated`, `already_present`, or an error class.
    pub classification: String,
}

/// One redacted unresolved classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedStreamDescriptorV1 {
    /// Opaque stable digest of the subject identity, not its raw identifier.
    pub subject_digest: String,
    /// Bounded operator-facing failure classification without content identity.
    pub classification: String,
}

/// Bounded unresolved page returned by the governed operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedStreamDescriptorPageV1 {
    /// Transport correlation identity.
    pub request_id: String,
    /// Platform-minted durable migration job identity.
    pub job_id: String,
    /// Redacted unresolved entries in deterministic order.
    pub entries: Vec<UnresolvedStreamDescriptorV1>,
    /// Opaque cursor for the next page, or absence at the end.
    pub next: Option<String>,
}

use super::{
    SCHEMA_DEPLOYMENT_ABI_V1, SchemaDeploymentClient, SchemaDeploymentErrorV1,
    SchemaDeploymentOperationV1, SchemaDeploymentRequestV1, SchemaDeploymentResponseV1, call_host,
};

impl SchemaDeploymentClient {
    /// Create one target-bound stream-descriptor migration job.
    pub fn start_stream_descriptor_migration(
        &self,
        request: StartStreamDescriptorMigrationRequestV1,
    ) -> Result<StreamDescriptorMigrationReceiptV1, SchemaDeploymentErrorV1> {
        call_stream_descriptor(SchemaDeploymentOperationV1::StartStreamDescriptorMigration(
            request,
        ))
    }

    /// Advance one bounded platform-owned inventory page.
    pub fn advance_stream_descriptor_migration(
        &self,
        request: AdvanceStreamDescriptorMigrationRequestV1,
    ) -> Result<StreamDescriptorMigrationReceiptV1, SchemaDeploymentErrorV1> {
        call_stream_descriptor(
            SchemaDeploymentOperationV1::AdvanceStreamDescriptorMigration(request),
        )
    }

    /// Read durable stream-descriptor migration progress.
    pub fn get_stream_descriptor_migration(
        &self,
        request: GetStreamDescriptorMigrationRequestV1,
    ) -> Result<StreamDescriptorMigrationReceiptV1, SchemaDeploymentErrorV1> {
        call_stream_descriptor(SchemaDeploymentOperationV1::GetStreamDescriptorMigration(
            request,
        ))
    }

    /// Read one redacted bounded unresolved page.
    pub fn list_unresolved_stream_descriptors(
        &self,
        request: ListUnresolvedStreamDescriptorsRequestV1,
    ) -> Result<UnresolvedStreamDescriptorPageV1, SchemaDeploymentErrorV1> {
        let operation = SchemaDeploymentOperationV1::ListUnresolvedStreamDescriptors(request);
        let response = call_host_request(operation, "stream descriptor unresolved request")?;
        match response {
            SchemaDeploymentResponseV1::UnresolvedStreamDescriptors { page } => Ok(page),
            SchemaDeploymentResponseV1::Error { error } => Err(error),
            _ => Err(local_error(
                "backend_unavailable",
                "schema deployment host returned the wrong receipt".into(),
            )),
        }
    }
}

fn call_stream_descriptor(
    operation: SchemaDeploymentOperationV1,
) -> Result<StreamDescriptorMigrationReceiptV1, SchemaDeploymentErrorV1> {
    match call_host_request(operation, "stream descriptor migration request")? {
        SchemaDeploymentResponseV1::StreamDescriptorMigration { receipt } => Ok(receipt),
        SchemaDeploymentResponseV1::Error { error } => Err(error),
        _ => Err(local_error(
            "backend_unavailable",
            "schema deployment host returned the wrong receipt".into(),
        )),
    }
}

fn call_host_request(
    operation: SchemaDeploymentOperationV1,
    description: &str,
) -> Result<SchemaDeploymentResponseV1, SchemaDeploymentErrorV1> {
    let request = SchemaDeploymentRequestV1 {
        abi: SCHEMA_DEPLOYMENT_ABI_V1.into(),
        operation,
    };
    let bytes = serde_json::to_vec(&request).map_err(|error| {
        local_error(
            "invalid_bundle",
            format!("failed to encode {description}: {error}"),
        )
    })?;
    call_host(&bytes)
}

pub(super) fn local_error(code: &str, message: String) -> SchemaDeploymentErrorV1 {
    SchemaDeploymentErrorV1 {
        code: code.into(),
        message,
        retryable: false,
        decision_id: None,
    }
}
