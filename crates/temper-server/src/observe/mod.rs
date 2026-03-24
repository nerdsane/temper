//! Observe API routes for developer tooling.
//!
//! These endpoints expose internal Temper state for the observability frontend.
//! They are only available when the `observe` feature is enabled.

mod agents;
mod entities;
pub(crate) mod evolution;
mod metrics;
mod refresh;
pub(crate) mod specs;
mod specs_helpers;
pub mod subprocess_verify;
mod verification;
pub(crate) mod wasm;
use axum::Router;

use axum::routing::{get, patch, post};
use serde::{Deserialize, Serialize};

use crate::state::ServerState;

/// Summary of a loaded spec.
#[derive(Serialize, Deserialize)]
pub struct SpecSummary {
    /// Tenant that owns this spec.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// Valid status states.
    pub states: Vec<String>,
    /// Action names.
    pub actions: Vec<String>,
    /// Initial state.
    pub initial_state: String,
    /// Verification status: "pending", "running", "passed", "failed", "partial".
    pub verification_status: String,
    /// Number of verification levels that passed (if completed).
    pub levels_passed: Option<usize>,
    /// Total number of verification levels (if completed).
    pub levels_total: Option<usize>,
}

/// Full spec detail.
#[derive(Serialize, Deserialize)]
pub struct SpecDetail {
    /// Entity type name.
    pub entity_type: String,
    /// Valid status states.
    pub states: Vec<String>,
    /// Initial state.
    pub initial_state: String,
    /// Action details.
    pub actions: Vec<ActionDetail>,
    /// Invariant details.
    pub invariants: Vec<InvariantDetail>,
    /// State variable declarations.
    pub state_variables: Vec<StateVarDetail>,
}

/// Detail of a single action.
#[derive(Serialize, Deserialize)]
pub struct ActionDetail {
    /// Action name.
    pub name: String,
    /// Action kind (input/output/internal).
    pub kind: String,
    /// States from which this action can fire.
    pub from: Vec<String>,
    /// Target state after firing.
    pub to: Option<String>,
    /// Guard conditions (Debug representation).
    pub guards: Vec<String>,
    /// Effects (Debug representation).
    pub effects: Vec<String>,
}

/// Detail of a single invariant.
#[derive(Serialize, Deserialize)]
pub struct InvariantDetail {
    /// Invariant name.
    pub name: String,
    /// Trigger states (empty = always checked).
    pub when: Vec<String>,
    /// Assertion expression.
    pub assertion: String,
}

/// Detail of a state variable.
#[derive(Serialize, Deserialize)]
pub struct StateVarDetail {
    /// Variable name.
    pub name: String,
    /// Variable type.
    pub var_type: String,
    /// Initial value.
    pub initial: String,
}

/// Entity instance summary.
#[derive(Serialize, Deserialize)]
pub struct EntityInstanceSummary {
    /// Tenant that owns this entity.
    pub tenant: String,
    /// Entity type.
    pub entity_type: String,
    /// Entity ID.
    pub entity_id: String,
    /// Actor liveness status (e.g. "active", "stopped").
    pub actor_status: String,
    /// Current state of the entity (e.g. "Open", "InProgress").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_state: Option<String>,
    /// ISO 8601 timestamp of the last state change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<String>,
}

/// Query parameters for the simulation endpoint.
#[derive(Deserialize)]
pub struct SimQueryParams {
    /// PRNG seed (default: 42).
    pub seed: Option<u64>,
    /// Max simulation ticks (default: 200).
    pub ticks: Option<u64>,
}

/// Query parameters for the SSE event stream.
#[derive(Deserialize)]
pub struct EventStreamParams {
    /// Filter by entity type.
    pub entity_type: Option<String>,
    /// Filter by entity ID.
    pub entity_id: Option<String>,
}

/// Build the observe router (mounted at /observe).
///
/// Read-only observation routes. Data-mutating endpoints live under `/api`.
/// Exception: POST /observe/verify/{entity} triggers computation but does not mutate state.
pub fn build_observe_router() -> Router<ServerState> {
    Router::new()
        .route("/specs", get(specs::handle_list_specs))
        .route("/specs/{entity}", get(specs::handle_get_spec_detail))
        .route("/entities", get(entities::handle_list_entities))
        .route(
            "/verify/{entity}",
            post(verification::handle_run_verification),
        )
        .route(
            "/simulation/{entity}",
            get(verification::handle_run_simulation),
        )
        .route("/paths/{entity}", get(verification::handle_get_paths))
        .route(
            "/entities/{entity_type}/{entity_id}/history",
            get(entities::handle_get_entity_history),
        )
        .route(
            "/entities/{entity_type}/{entity_id}/wait",
            get(entities::handle_wait_for_entity_state),
        )
        .route(
            "/entities/{entity_type}/{entity_id}/events",
            get(entities::handle_entity_event_stream),
        )
        .route("/events/stream", get(entities::handle_event_stream))
        .route(
            "/verification-status",
            get(verification::handle_verification_status),
        )
        .route(
            "/design-time/stream",
            get(verification::handle_design_time_stream),
        )
        .route("/workflows", get(verification::handle_workflows))
        .route("/health", get(metrics::handle_health))
        .route("/metrics", get(metrics::handle_metrics))
        .route("/trajectories", get(evolution::handle_trajectories))
        .route(
            "/evolution/records",
            get(evolution::handle_list_evolution_records),
        )
        .route(
            "/evolution/records/{id}",
            get(evolution::handle_get_evolution_record),
        )
        .route(
            "/evolution/insights",
            get(evolution::handle_list_evolution_insights),
        )
        .route("/agents", get(agents::handle_list_agents))
        .route(
            "/agents/{agent_id}/history",
            get(agents::handle_get_agent_history),
        )
        .route("/wasm/modules", get(wasm::handle_list_wasm_modules))
        .route("/wasm/invocations", get(wasm::handle_list_wasm_invocations))
        .route(
            "/wasm/modules/{module_name}",
            get(wasm::handle_get_wasm_module_info),
        )
        .route(
            "/evolution/unmet-intents",
            get(evolution::handle_unmet_intents),
        )
        .route(
            "/evolution/intent-evidence",
            get(evolution::handle_intent_evidence),
        )
        .route(
            "/evolution/feature-requests",
            get(evolution::handle_feature_requests),
        )
        .route(
            "/evolution/feature-requests/{id}",
            patch(evolution::handle_update_feature_request),
        )
        .route("/evolution/stream", get(evolution::handle_evolution_stream))
        .route("/refresh/stream", get(refresh::handle_refresh_stream))
}

#[cfg(test)]
#[path = "mod_test.rs"]
mod tests;
