//! Idempotent lifecycle commands and outcomes.

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
    /// Durable request identity.
    pub operation: SchemaOperationIdentity,
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
