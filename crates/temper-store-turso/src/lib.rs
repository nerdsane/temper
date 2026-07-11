//! # temper-store-turso
//!
//! Turso/libSQL storage backend for the Temper actor framework.
//!
//! This crate implements the [`EventStore`](temper_runtime::persistence::EventStore)
//! trait from `temper-runtime` using libSQL (Turso-compatible).

mod metrics;
mod retry;
pub mod router;
pub mod schema;
mod schema_event_history;
pub mod store;

/// Compute a SHA-256 hex digest of IOA spec content.
///
/// Used to detect whether a spec has changed since its last verification,
/// avoiding redundant (and expensive) verification cascade runs.
pub fn spec_content_hash(ioa_source: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(ioa_source.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[derive(Clone, Copy, Debug)]
pub struct TursoSpecVerificationUpdate<'a> {
    pub status: &'a str,
    pub verified: bool,
    pub levels_passed: Option<i32>,
    pub levels_total: Option<i32>,
    pub verification_result_json: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
pub struct TursoTrajectoryInsert<'a> {
    pub tenant: &'a str,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub action: &'a str,
    pub success: bool,
    pub from_status: Option<&'a str>,
    pub to_status: Option<&'a str>,
    pub error: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub authz_denied: Option<bool>,
    pub denied_resource: Option<&'a str>,
    pub denied_module: Option<&'a str>,
    pub source: Option<&'a str>,
    pub spec_governed: Option<bool>,
    pub created_at: &'a str,
    pub request_body: Option<&'a str>,
    pub intent: Option<&'a str>,
    pub matched_policy_ids: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
pub struct TursoWasmInvocationInsert<'a> {
    pub tenant: &'a str,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    pub module_name: &'a str,
    pub trigger_action: &'a str,
    pub callback_action: Option<&'a str>,
    pub success: bool,
    pub error: Option<&'a str>,
    pub duration_ms: u64,
    pub created_at: &'a str,
}

/// Values written atomically as one evolution record.
#[derive(Clone, Copy, Debug)]
pub struct TursoEvolutionRecordInsert<'a> {
    /// Tenant that owns the record.
    pub tenant: &'a str,
    /// Stable evolution record identifier.
    pub id: &'a str,
    /// Evolution record kind.
    pub record_type: &'a str,
    /// Current record status.
    pub status: &'a str,
    /// Principal that created the record.
    pub created_by: &'a str,
    /// Optional predecessor record identifier.
    pub derived_from: Option<&'a str>,
    /// Serialized record payload.
    pub data_json: &'a str,
}

/// Exact decision and policy values committed by one approval transaction.
#[derive(Clone, Copy, Debug)]
pub struct TursoPolicyApprovalCommit<'a> {
    /// Tenant that owns both rows.
    pub tenant: &'a str,
    /// Pending decision to transition.
    pub decision_id: &'a str,
    /// Serialized approved decision.
    pub approved_decision_json: &'a str,
    /// Policy row created by the decision.
    pub policy_id: &'a str,
    /// Approved Cedar source.
    pub cedar_text: &'a str,
    /// Principal that approved the decision.
    pub created_by: &'a str,
}

pub use metrics::init_metrics;
pub use router::{TenantRegistryRow, TenantStoreRouter, TenantUserRow};
pub use store::{
    ActionStats, AgentSummary, DesignTimeEventRow, EvolutionRecordRow, FeatureRequestRow,
    PolicyDenialPatternRow, PolicyRow, PublishedArtifactRow, PublishedArtifactUpsert,
    QueryProjectionUpsert, TursoBlobRow, TursoEventStore, TursoInstalledAppRow,
    TursoQueryProjectionRow, TursoSpecRow, TursoTenantConstraintRow, TursoTrajectoryRow,
    TursoWasmInvocationRow, TursoWasmModuleMetadataRow, TursoWasmModuleRow, UnmetIntentAggRow,
    ots::{OtsQueuedTrajectoryRow, OtsTrajectoryParams, OtsTrajectoryRow},
};
