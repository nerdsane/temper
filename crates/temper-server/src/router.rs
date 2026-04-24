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

    // Git clients (and GitHub-REST-compat clients) can't send
    // X-Tenant-Id; the whole point of the HttpEndpoint surface is
    // to terminate foreign wire protocols. So unlike the OData
    // router, the HttpEndpoint fallback always falls back to the
    // first registered non-system tenant when the header is absent.
    // Multi-tenant deployments that need strict tenant routing
    // should encode the tenant in the path prefix of their
    // HttpEndpoint rows (e.g. /{tenant}/{repo}.git/...).
    let tenant_id = match tenant_header {
        Some(t) if !t.is_empty() => TenantId::new(&t),
        _ => {
            let registry = state.registry.read().unwrap();
            let first_non_system = registry
                .tenant_ids()
                .into_iter()
                .find(|t| t.as_str() != "temper-system")
                .or_else(|| registry.tenant_ids().into_iter().next());
            match first_non_system {
                Some(t) => t.clone(),
                None => return http_404_response(uri.path()),
            }
        }
    };

    let table = state.http_endpoint_tables.table_for(&tenant_id).await;
    let matched = table.match_request(method.as_str(), uri.path()).await;

    let Some(route) = matched else {
        return http_404_response(uri.path());
    };

    dispatch_matched_route(state, tenant_id, method, uri, headers, _body, route).await
}

/// End-to-end dispatch: open an InboundExchange on the shared
/// HttpStreamRegistry, spawn tasks to pump axum body into the
/// guest-visible request-body handle and build an axum Response
/// body that drains the guest's response-body handle, fire the
/// WASM invocation, await the head, and stitch it into the
/// response.
async fn dispatch_matched_route(
    state: ServerState,
    tenant_id: TenantId,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
    route: crate::http_endpoint::MatchedRoute,
) -> Response {
    use temper_wasm::http_stream::HttpResponseHead;
    use temper_wasm::types::{HttpDispatchContext, WasmInvocationContext};

    // Resolve the integration module hash. The WASM module must
    // already be registered for this tenant (via app install).
    let module_hash: Option<String> = state
        .wasm_module_registry
        .read()
        .ok()
        .and_then(|reg| {
            reg.get_hash(&tenant_id, &route.route.integration_module)
                .map(|s| s.to_string())
        });
    let Some(module_hash) = module_hash else {
        tracing::warn!(
            tenant = %tenant_id.as_str(),
            integration = %route.route.integration_module,
            "HttpEndpoint match but integration module not registered"
        );
        return axum::http::Response::builder()
            .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
            .header("content-type", "application/json")
            .body(Body::from(format!(
                "{{\"error\":\"WASM integration '{}' not installed\"}}",
                route.route.integration_module
            )))
            .expect("response builder");
    };

    // Open an inbound exchange on the shared registry.
    let streams = state.http_stream_registry.clone();
    let exchange = streams.open_inbound_exchange().await;
    let guest_request_body = exchange.guest_request_body;
    let guest_response_body = exchange.guest_response_body;
    let kernel_request_body = exchange.kernel_request_body;
    let kernel_response_body = exchange.kernel_response_body;

    // Spawn task A: axum body → kernel_request_body handle.
    let pump_streams = streams.clone();
    tokio::spawn(async move {
        use axum::body::to_bytes;
        // TODO(slice 4): real streaming pump chunk-by-chunk. For
        // v0 we collect to bytes (capped) then forward — simpler
        // while we validate the dispatch path end-to-end.
        const MAX_REQ_BODY: usize = 64 * 1024 * 1024;
        match to_bytes(body, MAX_REQ_BODY).await {
            Ok(bytes) => {
                if !bytes.is_empty() {
                    let _ = pump_streams.write(kernel_request_body, bytes.to_vec()).await;
                }
                let _ = pump_streams.close(kernel_request_body).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "axum body → stream pump failed");
                let _ = pump_streams.close(kernel_request_body).await;
            }
        }
    });

    // Build the invocation context.
    let header_pairs: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), s.to_string())))
        .collect();
    let ctx = WasmInvocationContext {
        tenant: tenant_id.as_str().to_string(),
        entity_type: "HttpEndpoint".to_string(),
        entity_id: route.route.id.clone(),
        trigger_action: "HandleHttp".to_string(),
        trigger_params: serde_json::Value::Null,
        entity_state: serde_json::Value::Null,
        agent_id: None,
        session_id: None,
        integration_config: std::collections::BTreeMap::new(),
        trace_id: String::new(),
        http_request: Some(HttpDispatchContext {
            method: method.as_str().to_uppercase(),
            path: uri.path().to_string(),
            params: route.params.clone(),
            headers: header_pairs,
            principal_id: None,
            request_body_handle: guest_request_body.0,
            response_body_handle: guest_response_body.0,
        }),
    };

    // Build a per-request host that shares the registry.
    let secrets: std::collections::BTreeMap<String, String> = state
        .secrets_vault
        .as_ref()
        .map(|v| v.get_tenant_secrets(tenant_id.as_str()))
        .unwrap_or_default();
    let host: std::sync::Arc<dyn temper_wasm::WasmHost> = std::sync::Arc::new(
        temper_wasm::ProductionWasmHost::with_shared_streams(secrets, streams.clone()),
    );

    // Spawn task B: invoke the WASM module. Runs to completion
    // (guest writes head + body via FFI; we drain on the axum side).
    let limits = temper_wasm::types::WasmResourceLimits {
        max_duration: std::time::Duration::from_secs(route.route.timeout_secs as u64),
        ..Default::default()
    };
    let engine = state.wasm_engine.clone();
    let invoke_streams = std::sync::Arc::new(std::sync::RwLock::new(
        temper_wasm::stream::StreamRegistry::default(),
    ));
    tokio::spawn(async move {
        let _ = engine
            .invoke_with_blobs(
                &module_hash,
                &ctx,
                host,
                &limits,
                invoke_streams,
                std::collections::BTreeMap::new(),
            )
            .await;
    });

    // Await the guest's response head.
    let head: HttpResponseHead = match streams
        .await_inbound_response_head(guest_response_body)
        .await
    {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(error = %e, "HttpEndpoint: guest did not submit response head");
            return axum::http::Response::builder()
                .status(axum::http::StatusCode::BAD_GATEWAY)
                .body(Body::from("guest integration failed to submit response head"))
                .expect("response builder");
        }
    };

    // Build the axum response whose body drains the
    // kernel_response_body handle.
    let drain_streams = streams.clone();
    let body_stream = async_stream::stream! {
        loop {
            match drain_streams.read(kernel_response_body).await {
                Ok(chunk) if chunk.is_empty() => break,
                Ok(chunk) => yield Ok::<_, std::io::Error>(bytes::Bytes::from(chunk)),
                Err(_) => break,
            }
        }
    };

    let mut resp = axum::http::Response::builder().status(head.status);
    for (k, v) in &head.headers {
        resp = resp.header(k, v);
    }
    resp.body(Body::from_stream(body_stream))
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
