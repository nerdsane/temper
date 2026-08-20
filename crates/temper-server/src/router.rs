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
use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::Uri;
use axum::response::Response;
use temper_runtime::tenant::TenantId;

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
        );

    let router = Router::new()
        .nest("/tdata", tdata)
        .nest(
            "/api/v1/schema-deployments",
            crate::schema_deployment::router(),
        )
        .nest("/_admin", crate::admin::build_admin_router())
        .route("/temper-client.js", get(serve_temper_client))
        .route("/static/temper-client.js", get(serve_temper_client))
        .route("/genesis", get(crate::static_web::serve_genesis_web))
        .route("/genesis/", get(crate::static_web::serve_genesis_web))
        .route(
            "/genesis/{*path}",
            get(crate::static_web::serve_genesis_web),
        )
        .route(
            "/webhooks/{tenant}/{*path}",
            get(webhook_receiver::handle_webhook).post(webhook_receiver::handle_webhook),
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
            HeaderName::from_static("idempotency-key"),
            HeaderName::from_static("x-temper-principal-id"),
            HeaderName::from_static("x-temper-principal-kind"),
            HeaderName::from_static("x-temper-agent-role"),
            HeaderName::from_static("x-temper-agent-type"),
            HeaderName::from_static("x-temper-schema-scope-kind"),
            HeaderName::from_static("x-temper-schema-scope-id"),
            HeaderName::from_static("x-temper-request-id"),
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
    // to terminate foreign wire protocols. When the header is absent
    // the fallback prefers the registered `default` tenant — protocol
    // endpoint rows live there on single-operator deployments — and
    // only then any other non-system tenant. Picking "first
    // registered" made resolution depend on registry iteration order:
    // a production server whose extra tenants sort before "default"
    // resolved header-less git/REST requests to a tenant with an
    // empty route table and answered 404. Multi-tenant deployments
    // that need strict tenant routing should encode the tenant in the
    // path prefix of their HttpEndpoint rows
    // (e.g. /{tenant}/{repo}.git/...).
    let tenant_id = match tenant_header {
        Some(t) if !t.is_empty() => TenantId::new(&t),
        _ => {
            let Ok(registry) = state.registry.read() else {
                return http_404_response(uri.path());
            };
            match http_endpoint_fallback_tenant(&registry.tenant_ids()) {
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

/// Tenant for a header-less HttpEndpoint request: the registered
/// `default` tenant when present (deterministic — protocol endpoint
/// rows live there on single-operator deployments), else the first
/// non-system tenant, else any tenant at all.
fn http_endpoint_fallback_tenant<'a>(tenant_ids: &[&'a TenantId]) -> Option<&'a TenantId> {
    let default_tenant = TenantId::default();
    tenant_ids
        .iter()
        .copied()
        .find(|t| **t == default_tenant)
        .or_else(|| {
            tenant_ids
                .iter()
                .copied()
                .find(|t| t.as_str() != "temper-system")
        })
        .or_else(|| tenant_ids.first().copied())
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
    let module_hash: Option<String> = state.wasm_module_registry.read().ok().and_then(|reg| {
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
    // Streaming pump: each Frame::data() chunk is forwarded as it
    // arrives, so guests reading via the WASM SDK's request_body()
    // see bytes the moment axum hands them over rather than after
    // the whole request has been buffered. This is what makes the
    // ADR-0057 inbound exchange end-to-end streaming — without it,
    // even SDK-streaming guests are bounded by the buffered limit.
    let pump_streams = streams.clone();
    tokio::spawn(async move {
        use tokio_stream::StreamExt as _;
        let mut stream = body.into_data_stream();
        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(chunk) if chunk.is_empty() => {}
                Ok(chunk) => {
                    if pump_streams
                        .write(kernel_request_body, chunk.to_vec())
                        .await
                        .is_err()
                    {
                        tracing::warn!("axum body → stream pump: guest closed early");
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "axum body → stream pump failed");
                    break;
                }
            }
        }
        let _ = pump_streams.close(kernel_request_body).await;
    });

    // Build the invocation context.
    let header_pairs: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (k.as_str().to_string(), s.to_string()))
        })
        .collect();
    let route_params = git_route_params_for_http_dispatch(
        &route.route.integration_module,
        uri.path(),
        route.params.clone(),
    );
    let ctx = WasmInvocationContext {
        tenant: tenant_id.as_str().to_string(),
        entity_type: "HttpEndpoint".to_string(),
        entity_id: route.route.id.clone(),
        trigger_action: "HandleHttp".to_string(),
        wasm_module: Some(route.route.integration_module.clone()),
        trigger_params: serde_json::Value::Null,
        entity_state: serde_json::Value::Null,
        agent_id: None,
        session_id: None,
        integration_config: std::collections::BTreeMap::new(),
        trace_id: String::new(),
        workflow_root_entity_type: None,
        workflow_root_entity_id: None,
        workflow_run_id: None,
        http_request: Some(HttpDispatchContext {
            method: method.as_str().to_uppercase(),
            // Include the query string so guests can parse
            // `service=git-upload-pack` etc. from InboundHttp.path.
            path: match uri.query() {
                Some(q) => format!("{}?{}", uri.path(), q),
                None => uri.path().to_string(),
            },
            params: route_params.clone(),
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
        temper_wasm::ProductionWasmHost::with_shared_streams(secrets, streams.clone())
            .with_invocation_context(ctx.clone()),
    );

    // Spawn task B: invoke the WASM module. Runs to completion
    // (guest writes head + body via FFI; we drain on the axum side).
    let mut limits = temper_wasm::types::WasmResourceLimits {
        max_duration: std::time::Duration::from_secs(route.route.timeout_secs as u64),
        ..Default::default()
    };
    if let Some(max_fuel) = route.route.max_fuel {
        limits.max_fuel = max_fuel;
    }
    if let Some(max_memory) = route.route.max_memory {
        limits.max_memory = max_memory;
    }
    if let Some(max_response_bytes) = route.route.max_response_bytes {
        limits.max_response_bytes = max_response_bytes;
    }
    let engine = state.wasm_engine.clone();
    let invoke_streams = std::sync::Arc::new(std::sync::RwLock::new(
        temper_wasm::stream::StreamRegistry::default(),
    ));
    let invoke_hash = module_hash.clone();
    let invoke_integration = route.route.integration_module.clone();
    tracing::info!(
        endpoint_id = %route.route.id,
        integration = %invoke_integration,
        module_hash = %invoke_hash,
        request_body_handle = guest_request_body.0,
        response_body_handle = guest_response_body.0,
        "HttpEndpoint dispatch → invoking WASM"
    );

    if let Some(action_bridge) = route.route.action_bridge.clone() {
        let result = match engine
            .invoke_with_blobs(
                &invoke_hash,
                &ctx,
                host,
                &limits,
                invoke_streams,
                std::collections::BTreeMap::new(),
            )
            .await
        {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(
                    integration = %invoke_integration,
                    error = %e,
                    "WASM adapter failed before action bridge dispatch"
                );
                return action_bridge_error_response(
                    &action_bridge.response,
                    &[],
                    false,
                    &format!("adapter failed: {e}"),
                );
            }
        };

        return dispatch_action_bridge_result(
            state,
            tenant_id,
            headers,
            route_params,
            route.route.requires_auth,
            action_bridge,
            result,
        )
        .await;
    }

    let invoke_task = tokio::spawn(async move {
        match engine
            .invoke_with_blobs(
                &invoke_hash,
                &ctx,
                host,
                &limits,
                invoke_streams,
                std::collections::BTreeMap::new(),
            )
            .await
        {
            Ok(result) => {
                if !result.success {
                    tracing::warn!(
                        integration = %invoke_integration,
                        error = result.error.unwrap_or_default(),
                        "WASM integration returned success=false"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    integration = %invoke_integration,
                    error = %e,
                    "WASM integration failed to invoke"
                );
            }
        }
    });

    // Await the guest's response head — bounded by the route's
    // configured timeout so a bad guest doesn't wedge the request.
    let head_timeout = std::time::Duration::from_secs(route.route.timeout_secs as u64);
    let head_result = tokio::time::timeout(
        head_timeout,
        streams.await_inbound_response_head(guest_response_body),
    )
    .await;
    let head: HttpResponseHead = match head_result {
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "HttpEndpoint: guest did not submit response head");
            invoke_task.abort();
            return axum::http::Response::builder()
                .status(axum::http::StatusCode::BAD_GATEWAY)
                .body(Body::from(format!(
                    "guest integration failed to submit response head: {e}"
                )))
                .expect("response builder");
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = route.route.timeout_secs,
                "HttpEndpoint: guest timed out before submitting response head"
            );
            invoke_task.abort();
            return axum::http::Response::builder()
                .status(axum::http::StatusCode::GATEWAY_TIMEOUT)
                .body(Body::from(
                    "guest integration timed out before submitting response head",
                ))
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

async fn dispatch_action_bridge_result(
    state: ServerState,
    tenant_id: TenantId,
    headers: HeaderMap,
    route_params: std::collections::BTreeMap<String, String>,
    requires_auth: bool,
    bridge: crate::http_endpoint::HttpActionBridge,
    result: temper_wasm::types::WasmInvocationResult,
) -> Response {
    let callback_params = result.callback_params.clone();
    let refs = bridge_git_receive_pack_refs(&callback_params);
    let sideband = bridge_git_receive_pack_sideband(&callback_params);

    if !result.success {
        return action_bridge_error_response(
            &bridge.response,
            &refs,
            sideband,
            result
                .error
                .as_deref()
                .unwrap_or("adapter returned unsuccessful result"),
        );
    }

    if let Some(short_circuit) = bridge_short_circuit_response(&callback_params) {
        return short_circuit;
    }

    let entity_id = match render_action_bridge_template(&bridge.entity_id_template, &route_params) {
        Ok(id) => id,
        Err(e) => {
            return action_bridge_error_response(&bridge.response, &refs, sideband, &e);
        }
    };
    let action_params = bridge_action_params(&callback_params);

    let mut agent_ctx = crate::request_context::extract_agent_context(&headers);
    let explicit_principal = headers.contains_key("x-temper-principal-kind")
        || headers.contains_key("x-temper-principal-id")
        || headers.contains_key("x-temper-principal-scopes");
    // Protocol adapters (git wire, REST shims) authenticate callers whose
    // credentials are not X-Temper-* headers; their resolved principal
    // takes precedence over header context and the system fallback
    // (ADR-0138).
    agent_ctx.security_ctx = Some(
        if let Some(adapter_principal) = bridge_resolved_principal(&callback_params) {
            adapter_principal
        } else if requires_auth || explicit_principal {
            crate::authz::security_context_from_headers(
                &headers,
                agent_ctx.agent_id.as_deref(),
                agent_ctx.session_id.as_deref(),
                agent_ctx.agent_type.as_deref(),
            )
        } else {
            temper_authz::SecurityContext::system()
        },
    );
    if agent_ctx.idempotency_key.is_none() {
        agent_ctx.idempotency_key = action_params
            .get("ClientRequestId")
            .or_else(|| action_params.get("client_request_id"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }

    tracing::info!(
        tenant = %tenant_id.as_str(),
        entity_type = %bridge.entity_type,
        entity_id = %entity_id,
        action = %bridge.action,
        response = %bridge.response,
        "HttpEndpoint action bridge dispatching configured action"
    );

    let dispatch = state
        .dispatch_tenant_action_ext_typed(
            &tenant_id,
            &bridge.entity_type,
            &entity_id,
            &bridge.action,
            action_params,
            crate::state::DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
                // Await reactions like the OData action path
                // (dispatch_bound_action) does, so a composite action
                // dispatched through the bridge — git wire IngestPack,
                // PR merge — has its declared sub-write reactions applied
                // before the protocol response is returned. Without this
                // the reaction is spawned in the background and its
                // sub_writes are silently dropped.
                await_reactions: true,
            },
        )
        .await;

    match dispatch {
        Ok(response) if response.success => {
            action_bridge_success_response(&bridge.response, &refs, sideband)
        }
        Ok(response) => action_bridge_error_response(
            &bridge.response,
            &refs,
            sideband,
            response
                .error
                .as_deref()
                .unwrap_or("configured action failed"),
        ),
        Err(e) => action_bridge_error_response(&bridge.response, &refs, sideband, &e.to_string()),
    }
}

fn bridge_action_params(callback_params: &serde_json::Value) -> serde_json::Value {
    if let Some(params) = callback_params
        .get("action_params")
        .or_else(|| callback_params.get("ActionParams"))
    {
        return params.clone();
    }
    // Legacy passthrough adapters return params at the top level; the
    // ADR-0138 control keys must never leak into a dispatched action.
    let mut fallback = callback_params.clone();
    if let Some(map) = fallback.as_object_mut() {
        map.remove("bridge_principal");
        map.remove("bridge_response");
    }
    fallback
}

fn git_route_params_for_http_dispatch(
    integration_module: &str,
    request_path: &str,
    mut params: std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    if params.get("owner").is_some_and(|v| !v.is_empty())
        && params.get("repo").is_some_and(|v| !v.is_empty())
    {
        return params;
    }
    if !matches!(
        integration_module,
        "git_refs_advertise" | "git_upload_pack" | "git_receive_pack"
    ) {
        return params;
    }

    let trimmed = request_path.trim_start_matches('/');
    let Some((owner, rest)) = trimmed.split_once('/') else {
        return params;
    };
    let repo = rest
        .strip_suffix(".git/info/refs")
        .or_else(|| rest.strip_suffix(".git/git-upload-pack"))
        .or_else(|| rest.strip_suffix(".git/git-receive-pack"));
    let Some(repo) = repo else {
        return params;
    };
    if owner.is_empty() || repo.is_empty() {
        return params;
    }

    params
        .entry("owner".to_string())
        .or_insert_with(|| owner.to_string());
    params
        .entry("repo".to_string())
        .or_insert_with(|| repo.to_string());
    params
}

/// Adapter short-circuit response for action-bridge routes (ADR-0138).
///
/// Protocol adapters that must refuse a request before any action
/// dispatch — e.g. a 401 challenge on an unauthenticated push — return
/// `bridge_response` with `status`, optional string `headers`, and an
/// optional string `body`. Presence of the key always ends the request:
/// a well-formed value is returned verbatim; a malformed one fails
/// closed as 502 so an adapter's refusal can never decay into a
/// dispatch (which would otherwise run as the system fallback).
fn bridge_short_circuit_response(callback_params: &serde_json::Value) -> Option<Response> {
    let resp = callback_params.get("bridge_response")?;
    // Same passthrough guard as bridge_principal: control keys are
    // honored only in the structured result shape, so a legacy adapter
    // echoing client JSON as top-level params can never hand a caller
    // response control on the kernel origin. Fail closed, not through.
    if callback_params.get("action_params").is_none()
        && callback_params.get("ActionParams").is_none()
    {
        tracing::warn!(
            "bridge_response without explicit action_params fails closed (ADR-0138 structured shape required)"
        );
        return Some(
            axum::http::Response::builder()
                .status(axum::http::StatusCode::BAD_GATEWAY)
                .body(Body::from(
                    "adapter bridge_response requires the structured result shape",
                ))
                .expect("response builder"),
        );
    }
    Some(match build_bridge_short_circuit(resp) {
        Ok(response) => response,
        Err(reason) => {
            tracing::warn!(%reason, "malformed bridge_response from adapter; failing closed");
            axum::http::Response::builder()
                .status(axum::http::StatusCode::BAD_GATEWAY)
                .body(Body::from("adapter returned a malformed bridge_response"))
                .expect("response builder")
        }
    })
}

fn build_bridge_short_circuit(resp: &serde_json::Value) -> Result<Response, String> {
    let status_code = resp
        .get("status")
        .and_then(|v| v.as_u64())
        .ok_or("bridge_response.status must be a number")?;
    let status = u16::try_from(status_code)
        .ok()
        .and_then(|code| axum::http::StatusCode::from_u16(code).ok())
        .ok_or_else(|| format!("bridge_response.status {status_code} is not a valid status"))?;
    let mut builder = axum::http::Response::builder().status(status);
    if let Some(headers) = resp.get("headers").and_then(|v| v.as_object()) {
        for (name, value) in headers {
            let value = value
                .as_str()
                .ok_or_else(|| format!("bridge_response header {name} must be a string"))?;
            builder = builder.header(name, value);
        }
    }
    let body = resp
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    builder
        .body(Body::from(body))
        .map_err(|e| format!("bridge_response could not be built: {e}"))
}

/// Adapter-resolved principal for action-bridge dispatch (ADR-0138).
///
/// Adapters that authenticate protocol-native credentials (git Basic auth
/// mapping to a domain token entity) return `bridge_principal` with `kind`,
/// `id`, and optional `scopes`. Honored only alongside an explicit
/// `action_params` key: a passthrough adapter that echoes request content
/// into top-level params must never hand the caller an identity. Scopes are
/// token-like (`repo:push`, `force`); values containing whitespace, commas,
/// or semicolons would be split by the header-parsing path. The `system`
/// kind cannot be smuggled — `SecurityContext::from_headers` maps unknown
/// kinds to Customer.
fn bridge_resolved_principal(
    callback_params: &serde_json::Value,
) -> Option<temper_authz::SecurityContext> {
    let principal = callback_params.get("bridge_principal")?;
    if callback_params.get("action_params").is_none()
        && callback_params.get("ActionParams").is_none()
    {
        tracing::warn!(
            "bridge_principal without explicit action_params is ignored (ADR-0138 structured shape required)"
        );
        return None;
    }
    let kind = principal.get("kind").and_then(|v| v.as_str())?.trim();
    let id = principal.get("id").and_then(|v| v.as_str())?.trim();
    if kind.is_empty() || id.is_empty() {
        return None;
    }
    let mut header_pairs = vec![
        ("x-temper-principal-kind".to_string(), kind.to_string()),
        ("x-temper-principal-id".to_string(), id.to_string()),
    ];
    let scopes = principal
        .get("scopes")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|s| s.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    if !scopes.is_empty() {
        header_pairs.push(("x-temper-principal-scopes".to_string(), scopes));
    }
    Some(temper_authz::SecurityContext::from_headers(&header_pairs))
}

fn bridge_git_receive_pack_refs(callback_params: &serde_json::Value) -> Vec<String> {
    callback_params
        .get("git_receive_pack")
        .or_else(|| callback_params.get("GitReceivePack"))
        .and_then(|v| v.get("refs"))
        .or_else(|| callback_params.get("refs"))
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn bridge_git_receive_pack_sideband(callback_params: &serde_json::Value) -> bool {
    callback_params
        .get("git_receive_pack")
        .or_else(|| callback_params.get("GitReceivePack"))
        .and_then(|v| v.get("sideband"))
        .or_else(|| callback_params.get("sideband"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn render_action_bridge_template(
    template: &str,
    params: &std::collections::BTreeMap<String, String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let (prefix, after_open) = rest.split_at(open);
        out.push_str(prefix);
        let after_open = &after_open[1..];
        let Some(close) = after_open.find('}') else {
            return Err(format!(
                "unclosed action bridge placeholder in '{template}'"
            ));
        };
        let key = &after_open[..close];
        let value = params.get(key).ok_or_else(|| {
            format!("action bridge placeholder '{key}' was not captured by the route")
        })?;
        out.push_str(value);
        rest = &after_open[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn action_bridge_success_response(
    response_kind: &str,
    refs: &[String],
    sideband: bool,
) -> Response {
    match response_kind {
        "git-receive-pack" => git_receive_pack_response(refs, sideband, None),
        _ => axum::http::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"success":true}"#))
            .expect("response builder"),
    }
}

fn action_bridge_error_response(
    response_kind: &str,
    refs: &[String],
    sideband: bool,
    reason: &str,
) -> Response {
    match response_kind {
        "git-receive-pack" => git_receive_pack_response(refs, sideband, Some(reason)),
        _ => axum::http::Response::builder()
            .status(axum::http::StatusCode::BAD_GATEWAY)
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "success": false,
                    "error": reason,
                })
                .to_string(),
            ))
            .expect("response builder"),
    }
}

fn git_receive_pack_response(refs: &[String], sideband: bool, error: Option<&str>) -> Response {
    let reason = error.map(sanitize_git_report_text);
    let mut inner = Vec::new();
    let unpack_line = match reason.as_deref() {
        Some(reason) => format!("unpack {reason}\n"),
        None => "unpack ok\n".to_string(),
    };
    write_pkt_line(&mut inner, unpack_line.as_bytes());
    for refname in refs {
        let line = match reason.as_deref() {
            Some(reason) => format!("ng {refname} {reason}\n"),
            None => format!("ok {refname}\n"),
        };
        write_pkt_line(&mut inner, line.as_bytes());
    }
    write_pkt_flush(&mut inner);

    let body = if sideband {
        sideband_channel_one(inner)
    } else {
        inner
    };

    axum::http::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header("content-type", "application/x-git-receive-pack-result")
        .header("cache-control", "no-cache")
        .body(Body::from(body))
        .expect("response builder")
}

fn sideband_channel_one(inner: Vec<u8>) -> Vec<u8> {
    let mut response = Vec::new();
    for chunk in inner.chunks(65_515) {
        let mut payload = Vec::with_capacity(1 + chunk.len());
        payload.push(0x01);
        payload.extend_from_slice(chunk);
        write_pkt_line(&mut response, &payload);
    }
    write_pkt_flush(&mut response);
    response
}

fn write_pkt_line(out: &mut Vec<u8>, payload: &[u8]) {
    let len = payload.len() + 4;
    out.extend_from_slice(format!("{len:04x}").as_bytes());
    out.extend_from_slice(payload);
}

fn write_pkt_flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

fn sanitize_git_report_text(input: &str) -> String {
    let mut out = input
        .chars()
        .map(|c| match c {
            '\r' | '\n' | '\t' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect::<String>();
    if out.len() > 240 {
        out.truncate(240);
    }
    if out.trim().is_empty() {
        "failed".to_string()
    } else {
        out
    }
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
