//! Governed transport-neutral schema deployment service and HTTP adapter.

mod service;

use axum::Router;
use axum::routing::{get, post};

pub(crate) use service::GovernedSchemaDeploymentService;

/// Versioned HTTP routes backed by the shared governed service.
pub(crate) fn router() -> Router<crate::state::ServerState> {
    Router::new()
        .route("/", post(service::submit_http))
        .route("/{scope_id}/{digest}", get(service::get_http))
        .route("/{scope_id}/{digest}/verify", post(service::verify_http))
        .route(
            "/{scope_id}/{digest}/activate",
            post(service::activate_http),
        )
        .route("/{scope_id}/{digest}/retire", post(service::retire_http))
        .route("/migrations", post(service::start_migration_http))
        .route(
            "/migrations/{scope_id}/{job_id}",
            get(service::get_migration_http).post(service::retry_migration_http),
        )
        .route(
            "/stream-descriptor-migrations",
            post(service::start_stream_descriptor_migration_http),
        )
        .route(
            "/stream-descriptor-migrations/{job_id}",
            get(service::get_stream_descriptor_migration_http),
        )
        .route(
            "/stream-descriptor-migrations/{job_id}/advance",
            post(service::advance_stream_descriptor_migration_http),
        )
        .route(
            "/stream-descriptor-migrations/{job_id}/unresolved",
            post(service::list_unresolved_stream_descriptors_http),
        )
}
