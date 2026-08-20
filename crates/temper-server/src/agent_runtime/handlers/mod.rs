//! HTTP handlers for the Agent Runtime API.
//!
//! These handlers are thin wrappers that translate clean REST requests
//! into Temper IOA action dispatches against the `TemperAgent` entity.
//! They call `ServerState::dispatch_tenant_action` directly — no
//! self-referential HTTP round-trips.

use axum::routing::{get, post};

use crate::state::ServerState;

mod common;
mod control;
mod create;
mod delete;
mod status;

/// Build the `/v1/agent-runs` router.
pub fn build_agent_runtime_router() -> axum::Router<ServerState> {
    axum::Router::new()
        .route("/agent-runs", post(create::create_run))
        .route(
            "/agent-runs/{id}",
            get(status::get_run).delete(delete::delete_run),
        )
        .route("/agent-runs/{id}/steer", post(control::steer_run))
        .route("/agent-runs/{id}/cancel", post(control::cancel_run))
}
