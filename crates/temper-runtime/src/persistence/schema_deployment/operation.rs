//! Idempotent lifecycle commands and outcomes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{SchemaActivePointer, SchemaDeploymentRecord, SchemaScope};

/// Original verification lifecycle values retained for exact replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaVerificationReplay {
    /// Terminal verification status.
    pub status: super::SchemaDeploymentStatus,
    /// Verification fence returned to the caller.
    pub fence: u64,
    /// Lifecycle sequence returned to the caller.
    pub committed_sequence: u64,
    /// Immutable verifier receipt identity.
    pub verification_receipt_id: String,
}

/// Stable identity binding one mutating request to its canonical input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaOperationIdentity {
    /// Caller-selected idempotency key.
    pub idempotency_key: String,
    /// Canonical digest excluding transport retries.
    pub request_digest: String,
    /// Correlation identity returned in the adapter receipt.
    pub request_id: String,
}

/// Atomically claim or replay governed schema verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimSchemaVerification {
    /// Tenant that owns the private deployment.
    pub tenant: String,
    /// Exact task-local scope.
    pub scope: SchemaScope,
    /// Immutable bundle digest.
    pub bundle_digest: String,
    /// Deterministic logical claim time.
    pub logical_now: u64,
    /// Deterministic lease deadline.
    pub lease_expires_at: u64,
    /// Durable request identity.
    pub operation: SchemaOperationIdentity,
}

/// Result of an idempotent verification claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimSchemaVerificationOutcome {
    /// This request created or reclaimed the verification lease.
    Claimed(SchemaDeploymentRecord),
    /// This exact request already claimed work; current durable state is returned.
    Replayed(SchemaDeploymentRecord),
}

/// Atomically activate or replay one verified bundle pointer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivateSchemaBundle {
    /// Tenant that owns the private deployment.
    pub tenant: String,
    /// Exact task-local scope.
    pub scope: SchemaScope,
    /// Immutable target bundle digest.
    pub bundle_digest: String,
    /// Exact active predecessor expected by the caller.
    pub expected_predecessor: Option<String>,
    /// Exact deployment fence expected by the caller.
    pub expected_fence: u64,
    /// Immutable successful verification receipt.
    pub verification_receipt_id: String,
    /// Optional source-journal generation that must still match inside activation.
    pub stream_publication_fence: Option<StreamPublicationFence>,
    /// Durable request identity.
    pub operation: SchemaOperationIdentity,
}

/// Publication generation covered by stream-descriptor completion evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamPublicationFence {
    /// One task-scoped predecessor journal set.
    TaskScoped {
        /// Immutable predecessor bundle whose scoped journals were inventoried.
        source_bundle_digest: String,
        /// Exact count of predecessor journal events at migration completion.
        expected_write_version: u64,
        /// Exact descriptor-publishing action for every covered entity type.
        bindings: BTreeMap<String, String>,
    },
    /// Tenant-global entity types owned by one installed application closure.
    InstalledApplication {
        /// Stable installed-application identity.
        application_id: String,
        /// Immutable semantic digest of the application model closure.
        semantic_digest: String,
        /// Exact publication action and event generation for every covered type.
        bindings: BTreeMap<String, UnscopedStreamPublicationBinding>,
    },
}

/// One tenant-global publication action and its atomic event-generation fence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnscopedStreamPublicationBinding {
    /// Verified IOA action that publishes stream content for the entity type.
    pub publication_action: String,
    /// Canonical digest of the exact verified stream capability for this type.
    pub capability_digest: String,
    /// Exact count of unscoped events for the entity type at completion.
    pub expected_write_version: u64,
}

/// Result of idempotent activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivateSchemaBundleOutcome {
    /// The pointer changed in this transaction.
    Activated(SchemaActivePointer),
    /// The exact original pointer receipt was replayed.
    Replayed(SchemaActivePointer),
}

/// Atomically retire or replay one active bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetireSchemaBundle {
    /// Tenant that owns the private deployment.
    pub tenant: String,
    /// Exact task-local scope.
    pub scope: SchemaScope,
    /// Immutable active bundle digest.
    pub bundle_digest: String,
    /// Exact active fence expected by the caller.
    pub expected_fence: u64,
    /// Durable request identity.
    pub operation: SchemaOperationIdentity,
}

/// Result of idempotent retirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetireSchemaBundleOutcome {
    /// The active pointer was removed in this transaction.
    Retired(SchemaDeploymentRecord),
    /// The exact original retirement receipt was replayed.
    Replayed(SchemaDeploymentRecord),
}

/// Atomically reserve one migration retry before it may advance durable work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReserveSchemaMigrationRetry {
    /// Tenant that owns the migration job.
    pub tenant: String,
    /// Stable migration job identity.
    pub job_id: String,
    /// Durable request identity.
    pub operation: SchemaOperationIdentity,
}

/// Durable reservation returned for a migration retry request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaMigrationRetryReservation {
    /// Job state visible while the reservation transaction held its lock.
    pub job: super::SchemaMigrationJob,
    /// Job sequence before this retry was allowed to perform work.
    pub starting_sequence: u64,
    /// Whether this exact request was already reserved.
    pub replayed: bool,
    /// Original retry request identity returned by exact replay.
    pub accepted_request_id: String,
}
