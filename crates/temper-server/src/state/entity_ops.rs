//! Entity lifecycle methods for ServerState (spawn, query, delete, index).

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use tracing::{Instrument, instrument};

use temper_authz::{AuthzDecision, AuthzDenial, SecurityContext};
use temper_observe::wide_event;
use temper_runtime::actor::ActorRef;
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope, PersistenceError};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use super::dispatch::retry;
use super::{QueryProjectionReplayParityReport, ServerState, projection_backfill};
use crate::entity_actor::event_persistence::{
    apply_state_timeout_clock, encode_entity_event_payload, entity_event_type, is_entity_tombstone,
};
use crate::entity_actor::{
    EntityActor, EntityEvent, EntityMsg, EntityResponse, EntityState,
    recover_entity_state_from_store,
};
use crate::events::EntityStateChange;
use crate::registry::{VerificationDetail, VerificationStatus};
use crate::runtime_metrics;
use crate::storage::DataOnlyCreateRecord;

mod entities;
mod indexes;
mod lifecycle;
mod lookups;
mod maintenance;
mod mutations;
mod status;

fn actor_idle_timeout_secs() -> i64 {
    static ACTOR_IDLE_TIMEOUT: OnceLock<i64> = OnceLock::new();
    *ACTOR_IDLE_TIMEOUT.get_or_init(|| {
        std::env::var("TEMPER_ACTOR_IDLE_TIMEOUT") // determinism-ok: read once at startup
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(300)
    })
}

fn is_deleted_envelope(event: &PersistenceEnvelope) -> bool {
    is_entity_tombstone(&event.event_type, &event.payload)
}

fn record_projection_update_started(
    tenant: &TenantId,
    entity_type: &str,
    operation: &str,
    source: &str,
) {
    crate::query_projection_metrics::record_update_started(
        tenant.as_str(),
        entity_type,
        operation,
        source,
    );
}

fn record_projection_update_success(
    tenant: &TenantId,
    entity_type: &str,
    operation: &str,
    source: &str,
    sequence_nr: u64,
    started_at: Instant,
) {
    let duration = started_at.elapsed();
    crate::query_projection_metrics::record_update_duration(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        "ok",
        duration,
    );
    crate::query_projection_metrics::record_update_end_to_end_duration(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        "ok",
        duration,
    );
    crate::query_projection_metrics::record_update_applied_sequence(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        sequence_nr,
    );
}

fn record_projection_update_error(
    tenant: &TenantId,
    entity_type: &str,
    operation: &str,
    source: &str,
    started_at: Instant,
) {
    let duration = started_at.elapsed();
    crate::query_projection_metrics::record_update_duration(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        "error",
        duration,
    );
    crate::query_projection_metrics::record_update_end_to_end_duration(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        "error",
        duration,
    );
    crate::query_projection_metrics::record_update_error(
        tenant.as_str(),
        entity_type,
        operation,
        source,
    );
}

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
