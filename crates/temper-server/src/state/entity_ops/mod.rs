//! Entity lifecycle methods for ServerState (spawn, persist, query).

use std::collections::BTreeMap;

use crate::entity_actor::EntityResponse;
use crate::registry::VerificationDetail;

mod authz;
mod helpers;
mod index;
mod persist;
mod query;
mod spawn;

/// Error returned when the verification gate blocks an operation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VerificationGateError {
    /// The entity type that failed the gate.
    pub entity_type: String,
    /// Gate status: "pending", "running", or "failed".
    pub status: String,
    /// Human-readable message.
    pub message: String,
    /// Failed verification levels with details (only for "failed" status).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_levels: Option<Vec<FailedLevelInfo>>,
}

/// Information about a failed verification level.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FailedLevelInfo {
    /// Level name (e.g. "Level 2: Deterministic Simulation").
    pub level: String,
    /// Human-readable summary of the failure.
    pub summary: String,
    /// Detailed violation information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<VerificationDetail>>,
}

/// Snapshot of an entity as seen by Cedar authorization.
#[derive(Debug, Clone)]
pub(crate) struct AuthzResourceSnapshot {
    pub(crate) current_state: EntityResponse,
    pub(crate) resource_attrs: BTreeMap<String, serde_json::Value>,
}
