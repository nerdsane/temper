//! Axum router construction for the Temper Data API.

use axum::Router;
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderName};
use axum::http::{Method, StatusCode};
use axum::routing::get;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::dispatch;
use crate::events;
use crate::state::ServerState;
use crate::webhook_receiver;

const TEMPER_CLIENT_JS: &str = include_str!("../static/temper-client.js");

async fn serve_temper_client() -> (
    StatusCode,
    [(axum::http::header::HeaderName, &'static str); 2],
    &'static str,
) {
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/javascript"),
            (CACHE_CONTROL, "public, max-age=3600"),
        ],
        TEMPER_CLIENT_JS,
    )
}

/// Build the axum router with all Temper Data API routes.
///
/// Route structure:
/// - GET  /tdata                      → service document
/// - GET  /tdata/$metadata            → CSDL XML (tenant-scoped)
/// - GET  /tdata/$hints               → agent hints JSON
/// - GET  /tdata/$events              → SSE stream of entity state changes
/// - GET  /tdata/{*path}              → entity set / entity / navigation / function
/// - POST /tdata/{*path}              → create entity / bound action
/// - GET|POST /webhooks/{tenant}/{*path} → inbound webhook receiver
///
/// Tenant is extracted from the `X-Tenant-Id` header. Falls back to the
/// first registered tenant in the SpecRegistry.
pub fn build_router(state: ServerState) -> Router {
    let tdata = Router::new()
        .route("/", get(dispatch::handle_service_document))
        .route("/$metadata", get(dispatch::handle_metadata))
        .route("/$hints", get(dispatch::handle_hints))
        .route("/$events", get(events::handle_events))
        .route(
            "/{*path}",
            get(dispatch::handle_odata_get)
                .post(dispatch::handle_odata_post)
                .patch(dispatch::handle_odata_patch)
                .put(dispatch::handle_odata_put)
                .delete(dispatch::handle_odata_delete),
        );

    let router = Router::new()
        .nest("/tdata", tdata)
        .route("/temper-client.js", get(serve_temper_client))
        .route("/static/temper-client.js", get(serve_temper_client))
        .route(
            "/webhooks/{tenant}/{*path}",
            get(webhook_receiver::handle_webhook).post(webhook_receiver::handle_webhook),
        );

    #[cfg(feature = "observe")]
    let router = router.nest("/observe", crate::observe::build_observe_router());
    #[cfg(feature = "observe")]
    let router = router.nest("/api", crate::api::build_api_router());
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            CONTENT_TYPE,
            AUTHORIZATION,
            HeaderName::from_static("x-tenant-id"),
            HeaderName::from_static("x-agent-id"),
            HeaderName::from_static("x-session-id"),
            HeaderName::from_static("idempotency-key"),
            HeaderName::from_static("x-temper-principal-id"),
            HeaderName::from_static("x-temper-principal-kind"),
            HeaderName::from_static("x-temper-agent-role"),
        ]);

    router
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
}

#[cfg(test)]
#[path = "router_test.rs"]
mod tests;
