//! Axum router construction for the Temper Data API.

use axum::Router;
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderName};
use axum::http::{Method, StatusCode};
use axum::routing::{any, get, put};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::blobs;
use crate::events;
use crate::odata;
use crate::state::ServerState;
use crate::webhooks::receiver as webhook_receiver;

use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::extract::{Extension, State};
use axum::http::HeaderMap;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use temper_authz::AuthenticatedRequestContext;

const TEMPER_CLIENT_JS: &str = include_str!("../static/temper-client.js");
const INTERNAL_BLOB_BODY_LIMIT_BYTES: usize = 128 * 1024 * 1024;

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
/// - any method on /webhooks/{tenant}/{*path} → inbound webhook receiver
///
/// Tenant is extracted from the `X-Tenant-Id` header. Falls back to the
/// first registered tenant in the SpecRegistry.
///
/// Every request is one server span ([`with_request_span`]) around all of it.
pub fn build_router(state: ServerState) -> Router {
    with_request_span(build_routes(state))
}

/// The kernel's routes and guards, without the request's span. An embedder
/// that adds middleware of its own (the platform resolves credentials) adds it
/// here and then wraps the whole in [`with_request_span`], so that work is in
/// the request's trace too, not a trace of its own.
pub fn build_routes(state: ServerState) -> Router {
    let tdata = Router::new()
        .route("/", get(odata::handle_service_document))
        .route("/$metadata", get(odata::handle_metadata))
        .route("/$hints", get(odata::handle_hints))
        .route("/$events", get(events::handle_events))
        // Content-addressed binary ingest. Registered as a specific
        // route so axum routes it ahead of the generic `/{*path}`
        // catch-all, and so the body extractor can take a streaming
        // `Body` instead of a buffered `Bytes`.
        .route(
            "/Blobs/Temper.IngestRaw",
            axum::routing::post(odata::handle_blob_ingest_raw),
        )
        .route(
            "/{*path}",
            get(odata::handle_odata_get)
                .post(odata::handle_odata_post)
                .patch(odata::handle_odata_patch)
                .put(odata::handle_odata_put)
                .delete(odata::handle_odata_delete),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            odata::refuse_unavailable_tenant,
        ))
        // Outermost here, so a refused write's answer names its key too.
        .layer(axum::middleware::from_fn(odata::key_every_write));

    let router = Router::new()
        .nest("/tdata", tdata)
        .nest("/_admin", crate::admin::build_admin_router())
        .route("/temper-client.js", get(serve_temper_client))
        .route("/static/temper-client.js", get(serve_temper_client))
        .route("/genesis", get(crate::static_web::serve_genesis_web))
        .route("/genesis/", get(crate::static_web::serve_genesis_web))
        .route(
            "/genesis/{*path}",
            get(crate::static_web::serve_genesis_web),
        )
        // Any method: the webhook's signature, not the verb, admits a
        // request (ADR-0182).
        .route(
            "/webhooks/{tenant}/{*path}",
            any(webhook_receiver::handle_webhook),
        )
        .route(
            "/_internal/blobs/{*path}",
            put(blobs::put_blob)
                .get(blobs::get_blob)
                .layer(DefaultBodyLimit::max(INTERNAL_BLOB_BODY_LIMIT_BYTES)),
        )
        // ADR-0069 Phase 2 dispatcher fallback. Matched paths that
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
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            CONTENT_TYPE,
            AUTHORIZATION,
            HeaderName::from_static("x-tenant-id"),
            HeaderName::from_static("x-session-id"),
            HeaderName::from_static("x-repository-id"),
            HeaderName::from_static("x-expected-object-id"),
            HeaderName::from_static("idempotency-key"),
            // Trace context from the browser (Datadog RUM), so a click and the
            // request it makes are one trace.
            HeaderName::from_static("traceparent"),
            HeaderName::from_static("tracestate"),
        ])
        // A page reads the key its write was given, to send it again.
        .expose_headers([odata::IDEMPOTENCY_KEY]);

    router
        .layer(cors)
        // Axum applies the last layer first: strip caller authority before the
        // typed-context guard or any route handler observes the request.
        .layer(axum::middleware::from_fn(
            crate::authz::require_authenticated_request_context,
        ))
        .layer(axum::middleware::from_fn(
            crate::authz::strip_inbound_identity_headers,
        ))
        // Outermost: every request is inbound, refused or not.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::wake::note_inbound,
        ))
        .with_state(state)
}

/// One server span per request, as the outermost layer: everything the
/// request does, its guards and an embedder's middleware included, is inside
/// it, and a caller's `traceparent` is its parent.
pub fn with_request_span(router: Router) -> Router {
    router.layer(
        TraceLayer::new_for_http()
            .make_span_with(|request: &axum::http::Request<_>| {
                let span = tracing::info_span!(
                    "http.request",
                    otel.name = %format!("{} {}", request.method(), request.uri().path()),
                    http.method = %request.method(),
                    http.route = %request.uri().path(),
                    http.status_code = tracing::field::Empty,
                    otel.kind = "server",
                );
                // A caller's `traceparent` makes this server span its child, so the
                // whole request (handler, dispatch, storage, reactions) is in the
                // caller's trace. Nothing below re-parents.
                if let Some(parent) =
                    crate::request_context::remote_parent_from_headers(request.headers())
                {
                    tracing_opentelemetry::OpenTelemetrySpanExt::set_parent(&span, parent);
                }
                span
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
}

/// Fallback handler for paths not served by any built-in route.
/// Resolves the tenant from the request's credential, consults that tenant's
/// `HttpEndpointTable`, and (in slice 2) returns 501 on match, 404
/// otherwise. Slice 3 of K-1 Phase 2 replaces the 501 with a real
/// streaming dispatch into the bound WASM integration.
#[tracing::instrument(skip_all, fields(http.method = %method, http.route = %uri.path()))]
pub async fn http_endpoint_fallback(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    admitted: Option<Extension<crate::http_endpoint::AdmittedHttpEndpoint>>,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    _body: Body,
) -> Response {
    let Some(Extension(authenticated)) = authenticated else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let tenant_id = authenticated.tenant().clone();
    let route = if let Some(Extension(admitted)) = admitted {
        let Some(matched) = admitted.into_matched(&tenant_id, method.as_str(), uri.path()) else {
            tracing::warn!(tenant = %tenant_id, "HttpEndpoint admission binding mismatch");
            return StatusCode::UNAUTHORIZED.into_response();
        };
        matched
    } else {
        if authenticated.security_context().principal.id == "anonymous" {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let Some(table) = state.http_endpoint_tables.get(&tenant_id).await else {
            return http_404_response(uri.path());
        };
        let Some(matched) = table.match_request(method.as_str(), uri.path()).await else {
            return http_404_response(uri.path());
        };
        matched
    };
    if route.route.requires_auth && authenticated.security_context().principal.id == "anonymous" {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    dispatch_matched_route(state, authenticated, method, uri, headers, _body, route).await
}

mod action_bridge;
mod endpoint_dispatch;
#[cfg(test)]
use action_bridge::*;
use endpoint_dispatch::dispatch_matched_route;

fn http_404_response(path: &str) -> Response {
    axum::http::Response::builder()
        .status(axum::http::StatusCode::NOT_FOUND)
        .header("content-type", "application/json")
        .body(Body::from(format!(
            "{{\"error\":\"no route matches\",\"path\":\"{path}\"}}"
        )))
        .expect("response builder")
}

mod guest_headers;
#[cfg(test)]
pub(crate) use guest_headers::GUEST_FORBIDDEN_CREDENTIAL_HEADERS;
pub(crate) use guest_headers::{guest_visible_headers, is_credential_header};

#[cfg(test)]
#[path = "router_test.rs"]
mod tests;
