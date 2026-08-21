//! Observe API routes for developer tooling.
//!
//! These endpoints expose internal Temper state for the observability frontend.
//! They are only available when the `observe` feature is enabled.

mod agents;
mod entities;
pub(crate) mod evolution;
mod metrics;
mod projections;
mod reactions;
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
    /// Content hash of the IOA source this spec was parsed from.
    ///
    /// The authoritative value: it is what the spec store keeps, and what a
    /// conformance check compares a run's `metadata.spec_version` against.
    /// Reported here so a harness can record the digest the kernel holds
    /// rather than computing one from a spec file — a deploy path that
    /// rewrites line endings or re-emits the TOML hashes to something else
    /// entirely, and every conformance check for that run is then refused.
    ///
    /// Empty when the spec is not one this response could hash.
    #[serde(default)]
    pub spec_version: String,
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
    /// Parameters this action accepts, in spec order.
    #[serde(default)]
    pub params: Vec<ActionParamDetail>,
    /// Agent-facing hint from the spec (empty when the spec has none).
    #[serde(default)]
    pub hint: String,
}

/// A single action parameter: name plus its declared type ("string" default).
#[derive(Serialize, Deserialize)]
pub struct ActionParamDetail {
    /// Parameter name exactly as declared in the spec's `params` list.
    pub name: String,
    /// Declared parameter type, serialized under the JSON key `"type"`.
    /// Bare-named params carry `"string"`; typed params carry the spec's
    /// type name (e.g. `"uint64"`). This is an OPEN set — generated clients
    /// must not assume a closed enum.
    #[serde(rename = "type")]
    pub param_type: String,
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
    /// Whether the field participates in the durable query-plane projection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_indexed: Option<bool>,
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
            "/entities/{entity_type}/{entity_id}/events",
            get(entities::handle_entity_event_stream),
        )
        .route(
            "/entities/{entity_type}/{entity_id}/wait",
            get(entities::handle_wait_for_entity_state),
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
        .route("/reactions", get(reactions::handle_list_reactions))
        .route(
            "/reactions/{delivery_id}",
            get(reactions::handle_get_reaction),
        )
        .route(
            "/projections/replay-parity",
            get(projections::handle_replay_parity),
        )
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
