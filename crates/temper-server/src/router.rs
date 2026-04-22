//! Axum router construction for the Temper Data API.

use axum::Router;
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderName};
use axum::http::{Method, StatusCode};
use axum::routing::{get, put};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::blobs;
use crate::events;
use crate::odata;
use crate::state::ServerState;
use crate::webhooks::receiver as webhook_receiver;

use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::Uri;
use axum::response::Response;
use temper_runtime::tenant::TenantId;

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
        .route("/", get(odata::handle_service_document))
        .route("/$metadata", get(odata::handle_metadata))
        .route("/$hints", get(odata::handle_hints))
        .route("/$events", get(events::handle_events))
        .route(
            "/{*path}",
            get(odata::handle_odata_get)
                .post(odata::handle_odata_post)
                .patch(odata::handle_odata_patch)
                .put(odata::handle_odata_put)
                .delete(odata::handle_odata_delete),
        );

    let router = Router::new()
        .nest("/tdata", tdata)
        .nest("/_admin", crate::admin::build_admin_router())
        .route("/temper-client.js", get(serve_temper_client))
        .route("/static/temper-client.js", get(serve_temper_client))
        .route(
            "/webhooks/{tenant}/{*path}",
            get(webhook_receiver::handle_webhook).post(webhook_receiver::handle_webhook),
        )
        .route(
            "/_internal/blobs/{*path}",
            put(blobs::put_blob).get(blobs::get_blob),
        )
        // ADR-0056 Phase 2 dispatcher fallback. Matched paths that
        // aren't served by any built-in route above fall through to
        // this handler, which consults the per-tenant HttpEndpoint
        // table and dispatches to the bound WASM integration. Slice
        // 2 of Phase 2 returns 501 on match; slice 3 wires the
        // streaming dispatch on top of the ADR-0057 primitive.
        .fallback(http_endpoint_fallback);

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
            HeaderName::from_static("x-session-id"),
            HeaderName::from_static("idempotency-key"),
            HeaderName::from_static("x-temper-principal-id"),
            HeaderName::from_static("x-temper-principal-kind"),
            HeaderName::from_static("x-temper-agent-role"),
            HeaderName::from_static("x-temper-agent-type"),
        ]);

    router
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &axum::http::Request<_>| {
                    tracing::info_span!(
                        "http.request",
                        otel.name = %format!("{} {}", request.method(), request.uri().path()),
                        http.method = %request.method(),
                        http.route = %request.uri().path(),
                        http.status_code = tracing::field::Empty,
                        otel.kind = "server",
                    )
                })
                .on_response(
                    |response: &axum::http::Response<_>,
                     latency: std::time::Duration,
                     span: &tracing::Span| {
                        span.record("http.status_code", response.status().as_u16());
                        tracing::info!(
                            latency_ms = latency.as_millis() as u64,
                            status = response.status().as_u16(),
                            "response"
                        );
                    },
                ),
        )
        .layer(cors)
        .with_state(state)
}

/// Fallback handler for paths not served by any built-in route.
/// Resolves the tenant from `X-Tenant-Id`, consults the tenant's
/// `HttpEndpointTable`, and (in slice 2) returns 501 on match, 404
/// otherwise. Slice 3 of K-1 Phase 2 replaces the 501 with a real
/// streaming dispatch into the bound WASM integration.
#[tracing::instrument(skip_all, fields(http.method = %method, http.route = %uri.path()))]
async fn http_endpoint_fallback(
    State(state): State<ServerState>,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    _body: Body,
) -> Response {
    let tenant_header = headers
        .get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // In single-tenant mode, fall back to the first registered tenant
    // if the header is absent — same convention as the OData router.
    let tenant_id = match tenant_header {
        Some(t) if !t.is_empty() => TenantId::new(&t),
        _ => {
            if state.single_tenant_mode {
                let registry = state.registry.read().unwrap();
                match registry.tenant_ids().first() {
                    Some(t) => (*t).clone(),
                    None => return http_404_response(uri.path()),
                }
            } else {
                return axum::http::Response::builder()
                    .status(axum::http::StatusCode::BAD_REQUEST)
                    .body(Body::from(
                        "missing X-Tenant-Id header (required in multi-tenant mode)",
                    ))
                    .expect("response builder");
            }
        }
    };

    let table = state.http_endpoint_tables.table_for(&tenant_id).await;
    let matched = table.match_request(method.as_str(), uri.path()).await;

    let Some(route) = matched else {
        return http_404_response(uri.path());
    };

    // Slice 2 of K-1 Phase 2: route matches but dispatch isn't
    // wired. Return 501 with a descriptive payload so operators can
    // tell the difference from a genuine 404. Slice 3 replaces this
    // with real streaming dispatch.
    tracing::info!(
        tenant = %tenant_id.as_str(),
        endpoint_id = %route.route.id,
        integration = %route.route.integration_module,
        path_prefix = %route.route.path_prefix,
        "HttpEndpoint match (dispatch not yet wired — returning 501)"
    );
    axum::http::Response::builder()
        .status(axum::http::StatusCode::NOT_IMPLEMENTED)
        .header("content-type", "application/json")
        .body(Body::from(format!(
            "{{\"error\":\"HttpEndpoint dispatch not yet implemented\",\"endpoint_id\":\"{}\",\"integration\":\"{}\"}}",
            route.route.id, route.route.integration_module
        )))
        .expect("response builder")
}

fn http_404_response(path: &str) -> Response {
    axum::http::Response::builder()
        .status(axum::http::StatusCode::NOT_FOUND)
        .header("content-type", "application/json")
        .body(Body::from(format!(
            "{{\"error\":\"no route matches\",\"path\":\"{path}\"}}"
        )))
        .expect("response builder")
}

#[cfg(test)]
#[path = "router_test.rs"]
mod tests;
