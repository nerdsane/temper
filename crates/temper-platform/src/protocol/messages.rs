//! Platform event protocol.
//!
//! All internal platform events are typed via [`PlatformEvent`]. These replace
//! the old WebSocket messages — they're used for internal broadcast between
//! subsystems (deploy pipeline, evolution engine, optimization loop).

use serde::{Deserialize, Serialize};

/// Platform event broadcast envelope.
///
/// Emitted by the deploy pipeline, evolution engine, and optimization loop.
/// Subscribers (e.g., background tasks, logging) receive these via the
/// broadcast channel in [`PlatformState`](crate::state::PlatformState).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlatformEvent {
    /// A tenant was registered in the SpecRegistry.
    TenantRegistered {
        /// Tenant name.
        tenant: String,
        /// Number of entity types in the tenant.
        entity_count: usize,
    },

    /// A tenant was removed from the SpecRegistry.
    TenantRemoved {
        /// Tenant name.
        tenant: String,
    },

    /// Verification cascade status update.
    VerifyStatus {
        /// Tenant being verified.
        tenant: String,
        /// Which cascade level or entity.
        level: String,
        /// Current status.
        status: VerifyStepStatus,
        /// Human-readable summary.
        summary: String,
    },

    /// Deployment completed (success or failure).
    DeployStatus {
        /// Tenant that was deployed.
        tenant: String,
        /// Whether deployment succeeded.
        success: bool,
        /// Human-readable summary.
        summary: String,
    },

    /// Catalog entry published.
    CatalogPublished {
        /// Application ID in the catalog.
        app_id: String,
        /// Application name.
        name: String,
    },

    /// Evolution engine event (O-Record, I-Record, approval, etc.).
    EvolutionEvent {
        /// What kind of evolution event.
        event_type: String,
        /// Summary of the event.
        summary: String,
        /// Associated record ID.
        record_id: String,
    },

    /// An optimization recommendation was automatically applied.
    OptimizationApplied {
        /// Which optimizer produced this.
        optimizer: String,
        /// What action was taken.
        action: String,
        /// Estimated improvement (0.0–1.0).
        improvement: f64,
    },

    /// An optimization recommendation was proposed (needs approval).
    OptimizationProposed {
        /// Which optimizer produced this.
        optimizer: String,
        /// Description of the recommendation.
        description: String,
        /// Risk level.
        risk: String,
        /// Record ID for tracking.
        record_id: String,
    },

    /// Error event.
    Error {
        /// Error description.
        message: String,
    },
}

/// Status of a verification step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerifyStepStatus {
    /// Step is queued but not started.
    Pending,
    /// Step is currently running.
    Running,
    /// Step passed.
    Passed,
    /// Step failed.
    Failed,
}
