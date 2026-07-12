//! HttpEndpoint stream exchange helpers (ADR-0156 / ARN-207).
//!
//! Keeps `router.rs` under the readability line budget while centralizing
//! grant wiring and RAII cleanup for shared-registry stream handles.

use axum::body::Body;
use axum::response::Response;
use temper_wasm::host_trait::ProductionWasmHost;
use temper_wasm::http_stream::{HttpStreamRegistry, InboundExchange, StreamHandle};
use temper_wasm::types::WasmInvocationContext;

/// Open an inbound exchange or return a 503 when the global handle budget
/// is exhausted.
pub async fn open_inbound_exchange(
    streams: &HttpStreamRegistry,
) -> Result<InboundExchange, Response> {
    match streams.open_inbound_exchange().await {
        Ok(ex) => Ok(ex),
        Err(e) => {
            tracing::error!(error = %e, "HttpEndpoint: failed to open inbound stream exchange");
            Err(axum::http::Response::builder()
                .status(axum::http::StatusCode::SERVICE_UNAVAILABLE)
                .header("content-type", "application/json")
                .body(Body::from("{\"error\":\"stream handle budget exhausted\"}"))
                .expect("response builder"))
        }
    }
}

/// Build a per-request host sharing `streams`, granting only guest-facing
/// ends of the exchange (raw handles are not authority).
pub fn host_with_inbound_grants(
    secrets: std::collections::BTreeMap<String, String>,
    streams: std::sync::Arc<HttpStreamRegistry>,
    ctx: WasmInvocationContext,
    guest_request_body: StreamHandle,
    guest_response_body: StreamHandle,
) -> ProductionWasmHost {
    let host =
        ProductionWasmHost::with_shared_streams(secrets, streams).with_invocation_context(ctx);
    host.grant_stream_handles([guest_request_body, guest_response_body]);
    host
}

/// Close exchange ends after timeout/failure so registry slots do not linger.
pub async fn close_inbound_handles(streams: &HttpStreamRegistry, handles: [StreamHandle; 4]) {
    streams.close_handles(handles).await;
}
