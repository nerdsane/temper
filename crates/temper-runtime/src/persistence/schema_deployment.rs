//! Durable semantic contract for task-scoped schema deployment.

mod journal_identity;
mod operation;
mod store;

pub use journal_identity::*;
pub use operation::*;
pub use store::SchemaDeploymentStore;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// V1 scope kinds accepted by schema deployment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaScopeKind {
    /// One tenant-local task owns the active schema pointer.
    Task,
}

/// Stable tenant-local scope identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SchemaScope {
    /// Closed scope kind.
    pub kind: SchemaScopeKind,
    /// Opaque non-empty scope identifier.
    pub id: String,
}

/// Immutable schema identity carried by one scoped entity execution.
///
/// Absence of this value is the explicit tenant-global compatibility path.
/// Scoped recovery must compare the complete value and must never infer it
/// from the registry's current active pointer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SchemaExecutionPin {
    /// Exact tenant-local scope that selected the bundle.
    pub scope: SchemaScope,
    /// Immutable canonical bundle digest used by the actor.
    pub bundle_digest: String,
}

/// Immutable schema evidence committed with one entity action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaEventPin {
    /// Actor-level scope and bundle identity.
    pub execution: SchemaExecutionPin,
    /// Stable digest identifying the action inside the immutable bundle.
    pub action_digest: String,
}

/// Immutable canonical artifacts stored under one bundle digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaBundleRecord {
    /// Tenant that owns the private bundle.
    pub tenant: String,
    /// Scope whose lifecycle may activate this bundle.
    pub scope: SchemaScope,
    /// Canonical lowercase `sha256:<hex>` bundle identity.
    pub digest: String,
    /// Optional immutable predecessor digest.
    pub predecessor_digest: Option<String>,
    /// Canonical CSDL XML bytes represented as UTF-8.
    pub canonical_csdl: String,
    /// Fully-qualified entity type to canonical IOA TOML.
    pub canonical_ioa: BTreeMap<String, String>,
    /// Stable Cedar artifact name to canonical source.
    pub cedar_policies: BTreeMap<String, String>,
    /// Stable WASM logical name to immutable module digest.
    pub wasm_module_digests: BTreeMap<String, String>,
    /// Optional migration module logical name used for durable artifact lookup.
    pub migration_module_name: Option<String>,
    /// Optional migration module digest.
    pub migration_module_digest: Option<String>,
    /// Optional closed migration ABI version.
    pub migration_abi_version: Option<String>,
    /// Canonical serialized positive verification and migration budgets.
    pub canonical_budgets: String,
}

/// Monotonic deployment lifecycle from ADR-0159.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaDeploymentStatus {
    Submitted,
    Verifying,
    Verified,
    Activating,
    Active,
    Retiring,
    Retired,
    Rejected,
}

/// Durable lifecycle row for one immutable bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaDeploymentRecord {
    /// Immutable bundle record.
    pub bundle: SchemaBundleRecord,
    /// Current monotonic lifecycle status.
    pub status: SchemaDeploymentStatus,
    /// Monotonic worker/activation fencing token.
    pub fence: u64,
    /// Logical lease deadline for a verification worker.
    pub lease_expires_at: Option<u64>,
    /// Immutable successful or failed verification receipt identity.
    pub verification_receipt_id: Option<String>,
    /// Original verifier result retained for durable idempotent replay.
    #[serde(default)]
    pub verification_replay: Option<SchemaVerificationReplay>,
    /// Original active-pointer receipt retained for durable idempotent replay.
    #[serde(default)]
    pub activation_pointer: Option<SchemaActivePointer>,
    /// Monotonic sequence committed with every lifecycle mutation.
    pub committed_sequence: u64,
    /// Original accepted request identity returned by idempotent replays.
    pub accepted_request_id: String,
    /// Original verification request identity, when verification was claimed.
    #[serde(default)]
    pub verification_request_id: Option<String>,
    /// Original retirement request identity, when retirement committed.
    #[serde(default)]
    pub retirement_request_id: Option<String>,
}

impl SchemaDeploymentRecord {
    /// Reconstruct the exact lifecycle row returned by successful verification.
    pub fn verification_replay_record(&self) -> Option<Self> {
        let replay = self.verification_replay.as_ref()?;
        let mut record = self.clone();
        record.status = replay.status;
        record.fence = replay.fence;
        record.committed_sequence = replay.committed_sequence;
        record.verification_receipt_id = Some(replay.verification_receipt_id.clone());
        Some(record)
    }
}

/// Atomic active pointer read by scoped registry lookups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaActivePointer {
    /// Tenant owning the scope.
    pub tenant: String,
    /// Scope resolved by the caller.
    pub scope: SchemaScope,
    /// Complete immutable bundle visible after the pointer transaction.
    pub bundle_digest: String,
    /// Bundle that was active immediately before this pointer.
    pub predecessor_digest: Option<String>,
    /// Monotonic activation fence.
    pub fence: u64,
    /// Monotonic committed sequence.
    pub committed_sequence: u64,
    /// Original activation or cutover request identity.
    #[serde(default)]
    pub accepted_request_id: String,
}

/// Idempotent submit command whose bundle and key must co-commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitSchemaBundle {
    /// Immutable bundle to insert.
    pub bundle: SchemaBundleRecord,
    /// Caller-provided bounded idempotency key.
    pub idempotency_key: String,
    /// Digest of the complete canonical submit request.
    pub request_digest: String,
    /// Caller request identity persisted with the original receipt.
    pub request_id: String,
}

/// Result of an idempotent bundle submission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubmitSchemaBundleOutcome {
    /// This transaction inserted the immutable bundle and lifecycle row.
    Created(SchemaDeploymentRecord),
    /// An identical committed request was returned without another write.
    Replayed(SchemaDeploymentRecord),
}

/// Durable verifier result bound to one claim fence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaVerificationReceipt {
    /// Stable immutable receipt identity.
    pub id: String,
    /// Verifier ABI/version used for the decision.
    pub verifier_version: String,
    /// Digest of every verifier input.
    pub input_digest: String,
    /// Whether every required verification level passed.
    pub passed: bool,
}

/// Monotonic lifecycle for one shadow migration job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaMigrationStatus {
    /// Accepted but not yet claimed by a worker.
    Submitted,
    /// A fenced worker is scanning and transforming source entities.
    Migrating,
    /// The complete shadow set is awaiting validation.
    Validating,
    /// Validation passed and the job may atomically cut over.
    Ready,
    /// The target pointer is active; recovery may only move forward.
    CutOver,
    /// Post-cutover actor eviction and acknowledgement completed.
    Completed,
    /// The immutable job or transformed output was rejected.
    Rejected,
}

/// Durable positive budgets consumed by one migration job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaMigrationBudgets {
    /// Fuel available to each transformed entity.
    pub fuel_per_entity: u64,
    /// Linear-memory pages available to each invocation.
    pub memory_pages: u32,
    /// Maximum encoded input bytes per entity.
    pub input_bytes: u32,
    /// Maximum encoded output bytes per entity.
    pub output_bytes: u32,
    /// Maximum entities committed by one batch.
    pub entities_per_batch: u32,
    /// Maximum entities consumed by the whole job.
    pub total_entities: u64,
    /// Maximum durable batches consumed by the whole job.
    pub total_batches: u64,
    /// Maximum fenced claims across crash recovery.
    pub attempts: u32,
}

/// Immutable request that creates one migration job and idempotency record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSchemaMigration {
    /// Deterministic job identity.
    pub job_id: String,
    /// Tenant owning both bundles and all migrated state.
    pub tenant: String,
    /// Exact task-local scope.
    pub scope: SchemaScope,
    /// Currently active immutable source bundle.
    pub source_bundle_digest: String,
    /// Verified immutable target bundle.
    pub target_bundle_digest: String,
    /// Successful target verification receipt.
    pub verification_receipt_id: String,
    /// Active source pointer fence captured when the job was accepted.
    pub source_expected_fence: u64,
    /// Immutable migration module logical name.
    pub module_name: String,
    /// Immutable migration module digest.
    pub module_digest: String,
    /// Host-captured accepted authority, never guest-provided.
    pub accepted_authority_json: String,
    /// Positive execution and recovery budgets.
    pub budgets: SchemaMigrationBudgets,
    /// Caller-provided bounded idempotency key.
    pub idempotency_key: String,
    /// Digest of every canonical request field.
    pub request_digest: String,
    /// Original request identity returned on replay.
    pub request_id: String,
}

/// Durable migration job, cursor, lease, fence, and budget accounting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaMigrationJob {
    /// Immutable creation request.
    pub command: CreateSchemaMigration,
    /// Monotonic job lifecycle.
    pub status: SchemaMigrationStatus,
    /// Monotonic worker/cutover fencing token.
    pub fence: u64,
    /// Logical lease deadline for the current worker.
    pub lease_expires_at: Option<u64>,
    /// Exclusive stable `(entity_type, entity_id)` scan cursor.
    pub scan_cursor: Option<(String, String)>,
    /// Whether the complete source journal set was scanned.
    pub scan_complete: bool,
    /// Source event sequence caught up in shadow state.
    pub catch_up_sequence: u64,
    /// Entities whose transformation budget was consumed.
    pub consumed_entities: u64,
    /// Durable batches whose budget was consumed.
    pub consumed_batches: u64,
    /// Claims whose attempt budget was consumed.
    pub consumed_attempts: u32,
    /// Immutable validation receipt enabling cutover.
    pub validation_receipt_id: Option<String>,
    /// Immutable terminal migration receipt.
    pub migration_receipt_id: Option<String>,
    /// Monotonic committed sequence.
    pub committed_sequence: u64,
}

/// Result of an idempotent migration creation transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CreateSchemaMigrationOutcome {
    /// This transaction inserted the job and idempotency mapping.
    Created(SchemaMigrationJob),
    /// An identical committed request returned the original job.
    Replayed(SchemaMigrationJob),
}

/// One transformed entity kept invisible until cutover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaMigrationShadowRow {
    /// Fully-qualified entity type.
    pub entity_type: String,
    /// Tenant-local entity identity.
    pub entity_id: String,
    /// Source sequence transformed by this row.
    pub source_sequence: u64,
    /// Canonical target state object.
    pub canonical_state_json: String,
    /// Digest of the complete migration input.
    pub input_digest: String,
    /// Digest of the complete migration output.
    pub output_digest: String,
    /// Target journal event committed atomically with this shadow row.
    pub target_event: super::PersistenceEnvelope,
}

/// Immutable replay evidence for one committed shadow batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaMigrationBatchReceipt {
    /// Stable batch identity.
    pub id: String,
    /// Cursor expected before applying the batch.
    pub source_cursor: Option<(String, String)>,
    /// Cursor visible after applying the batch.
    pub next_cursor: Option<(String, String)>,
    /// Digest over ordered source rows.
    pub input_digest: String,
    /// Digest over ordered shadow rows.
    pub output_digest: String,
    /// Number of rows in this batch.
    pub row_count: u32,
}

/// Atomic command that writes shadow rows, cursor, budgets, and receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitSchemaMigrationBatch {
    /// Stable job identity.
    pub job_id: String,
    /// Current worker fence.
    pub expected_fence: u64,
    /// Exclusive cursor the worker claimed.
    pub expected_cursor: Option<(String, String)>,
    /// Cursor after the ordered rows in this batch.
    pub next_cursor: Option<(String, String)>,
    /// True when no source row exists after `next_cursor`.
    pub scan_complete: bool,
    /// True when concurrent source writes require a fresh full keyset pass.
    pub restart_scan: bool,
    /// Source bundle write version observed for the current complete pass.
    pub observed_source_write_version: u64,
    /// Ordered transformed rows.
    pub rows: Vec<SchemaMigrationShadowRow>,
    /// Immutable replay receipt.
    pub receipt: SchemaMigrationBatchReceipt,
}

/// Immutable validation or terminal rejection over the migration shadow state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaMigrationValidationReceipt {
    /// Stable validation identity.
    pub id: String,
    /// Digest of the complete ordered shadow state.
    pub shadow_digest: String,
    /// Event sequence caught up before validation.
    pub caught_up_sequence: u64,
    /// Whether all target schema and typed-reference checks passed.
    pub passed: bool,
}

/// Stable semantic storage failures shared by every backend.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaDeploymentStoreError {
    /// Required bounded/canonical input was invalid.
    #[error("invalid schema deployment input: {0}")]
    InvalidInput(String),
    /// One key was reused with different canonical input.
    #[error("idempotency_conflict")]
    IdempotencyConflict,
    /// The requested immutable deployment does not exist.
    #[error("schema deployment not found")]
    NotFound,
    /// The lifecycle state does not enable the requested operation.
    #[error("invalid_lifecycle_transition")]
    InvalidLifecycleTransition,
    /// The active pointer is not the expected predecessor.
    #[error("predecessor_mismatch")]
    PredecessorMismatch,
    /// A worker or caller lost its fencing token.
    #[error("stale_fence")]
    StaleFence,
    /// The named verification receipt is absent or unsuccessful.
    #[error("verification_failed")]
    VerificationFailed,
    /// The declared migration budget was consumed before commit.
    #[error("migration_budget_exhausted")]
    MigrationBudgetExhausted,
    /// Immutable migration input, replay evidence, or validation was rejected.
    #[error("migration_rejected")]
    MigrationRejected,
    /// The authoritative transaction failed before commit.
    #[error("backend_unavailable: {0}")]
    BackendUnavailable(String),
}
