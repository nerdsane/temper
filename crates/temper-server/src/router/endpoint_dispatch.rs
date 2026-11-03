//! Dispatch of a matched HttpEndpoint route into its WASM integration.

use super::action_bridge::{
    action_bridge_error_response, dispatch_action_bridge_result, git_route_params_for_http_dispatch,
};
use super::*;
use tracing::Instrument as _;

/// End-to-end dispatch: open an InboundExchange on the shared
/// HttpStreamRegistry, spawn tasks to pump axum body into the
/// guest-visible request-body handle and build an axum Response
/// body that drains the guest's response-body handle, fire the
/// WASM invocation, await the head, and stitch it into the
/// response.
pub(super) async fn dispatch_matched_route(
    state: ServerState,
    authenticated: AuthenticatedRequestContext,
    method: axum::http::Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
    route: crate::http_endpoint::MatchedRoute,
) -> Response {
    if let Some(config) = route.route.native.clone() {
        return crate::http_endpoint::native::dispatch(
            &state,
            crate::http_endpoint::native::NativeRequest {
                authenticated,
                method,
                uri,
                headers,
                body,
                matched: route,
            },
            config,
        )
        .await;
    }
    use temper_wasm::http_stream::HttpResponseHead;
    use temper_wasm::types::{HttpDispatchContext, WasmInvocationContext};
    let tenant_id = authenticated.tenant().clone();

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

    let deadline = crate::http_endpoint::budget::ExchangeDeadline::new(
        std::time::Duration::from_secs(u64::from(route.route.timeout_secs)),
    );

    // Startup recovery restores the module registry without compiling the modules: compile this one
    // now, as an entity integration does, or answer at once, before any admission action runs.
    if let Err(response) = crate::http_endpoint::ensure_integration_module(
        &state,
        &tenant_id,
        &route.route.integration_module,
        &module_hash,
        &deadline,
    )
    .await
    {
        return response;
    }

    // Run checks through OData/Cedar/IOA with the original authenticated caller.
    // The module cannot select or forge this identity, and receives only the
    // successful action responses, never a reusable caller credential.
    let admitted = match deadline
        .wait(crate::http_endpoint::admission::admit(
            &state,
            &authenticated,
            &route.route.id,
            &route.params,
            route.route.admission_actions.clone(),
        ))
        .await
    {
        Ok(Ok(admitted)) => admitted,
        Ok(Err(response)) => return response,
        Err(_) => {
            return crate::response::odata_error(
                StatusCode::GATEWAY_TIMEOUT,
                "AdmissionTimeout",
                "Endpoint action checks timed out",
            )
            .into_response();
        }
    };

    // An install can replace (and drop) the module while admission runs, and
    // a module restored at boot compiles on first use, within the deadline.
    let invoke_hash = match deadline
        .wait(state.module_hash_to_invoke(
            &tenant_id,
            &route.route.integration_module,
            &module_hash,
        ))
        .await
    {
        Ok(Ok(hash)) => hash,
        Ok(Err(error)) => {
            tracing::warn!(
                integration = %route.route.integration_module,
                error = %error,
                "HttpEndpoint module could not be loaded"
            );
            module_hash.clone()
        }
        Err(_) => {
            return crate::response::odata_error(
                StatusCode::GATEWAY_TIMEOUT,
                "ModuleLoadTimeout",
                "Endpoint module did not load in time",
            )
            .into_response();
        }
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
    let pump_task = async move {
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
    };
    // The pump records no spans and can wait on a guest that stops reading, so it holds no request span:
    // a span it held would stay open, and its trace would never be sent, for as long as the pump waits.
    tokio::spawn(pump_task); // determinism-ok: detached HTTP body pump, not simulation state

    // Build the invocation context. Guest-visible headers go through
    // `guest_visible_headers` so caller credentials are stripped in one audited
    // place (ARN-208).
    let header_pairs: Vec<(String, String)> =
        guest_visible_headers(&headers, route.route.forwards_credential);
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
        entity_state: serde_json::json!(admitted),
        agent_id: Some(authenticated.security_context().principal.id.clone()),
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
            principal_id: Some(authenticated.security_context().principal.id.clone()),
            request_body_handle: guest_request_body.0,
            response_body_handle: guest_response_body.0,
        }),
        trigger_principal: None,
    };

    // Build the canonical Cedar-gated host chain while retaining the inbound
    // exchange's shared stream registry.
    let host = match crate::state::authorized_http_endpoint_host(
        &state,
        &tenant_id,
        &route.route.integration_module,
        &ctx,
        streams.clone(),
    ) {
        Ok(host) => host,
        Err(error) => {
            tracing::error!(%error, "failed to construct authorized HttpEndpoint host");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Spawn task B: invoke the WASM module. Runs to completion
    // (guest writes head + body via FFI; we drain on the axum side).
    let mut limits = temper_wasm::types::WasmResourceLimits {
        max_duration: deadline.remaining(),
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
            authenticated,
            headers,
            route_params,
            action_bridge,
            result,
        )
        .await;
    }

    let head_streams = streams.clone();
    let invoke_future = async move {
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
        // A guest that ended without a head never will submit one: the request answers now,
        // not at the route's timeout. A head already submitted is kept.
        head_streams
            .abandon_inbound_response_head(guest_response_body)
            .await;
    };
    // The module runs in its own task but is still this request's work, so it stays in the request's span.
    let invoke_task = tokio::spawn(invoke_future.in_current_span()); // determinism-ok: detached WASM invocation task, not simulation state

    // Await the guest's response head — bounded by the route's
    // configured timeout so a bad guest doesn't wedge the request.
    let head_result = deadline
        .wait(streams.await_inbound_response_head(guest_response_body))
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
