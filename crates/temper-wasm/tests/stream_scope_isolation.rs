//! ARN-207 exploit regression: cross-invocation WASM stream isolation.
//!
//! `ServerState` shares one process-global `HttpStreamRegistry` across every
//! request, and `StreamHandle`s are small sequential `u32`s. A malicious guest
//! can therefore guess a handle belonging to another tenant's in-flight request
//! and read its body, inject into its response, or close its stream.
//!
//! These tests mirror the server wiring: two per-request `ProductionWasmHost`
//! instances share one registry (as `ServerState.http_stream_registry` does),
//! each standing in for a different tenant's invocation. The attacker host
//! operates on a handle it never received. Each op MUST be denied.

use std::collections::BTreeMap;
use std::sync::Arc;

use temper_wasm::WasmHost;
use temper_wasm::host_trait::ProductionWasmHost;
use temper_wasm::http_stream::{HttpStreamRegistry, StreamError};

/// Build an attacker host that shares the victim's registry — exactly how the
/// server hands every per-request host a clone of the one global registry.
fn attacker_host(registry: Arc<HttpStreamRegistry>) -> ProductionWasmHost {
    ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry)
}

#[tokio::test]
async fn guest_cannot_read_another_invocations_request_body() {
    let registry = Arc::new(HttpStreamRegistry::new());

    // Victim invocation: kernel opens an inbound exchange and pumps the
    // victim's request body into it (as the axum body pump does).
    let victim = registry.open_inbound_exchange().await;
    registry
        .write(victim.kernel_request_body, b"victim-tenant-secret".to_vec())
        .await
        .unwrap();

    // Attacker guesses the victim's guest-facing read handle.
    let attacker = attacker_host(registry.clone());
    let stolen = attacker.http_stream_read(victim.guest_request_body).await;

    assert_eq!(
        stolen,
        Err(StreamError::InvalidHandle),
        "SECURITY: attacker read another invocation's request body: {stolen:?}"
    );
}

#[tokio::test]
async fn guest_cannot_inject_into_another_invocations_response() {
    let registry = Arc::new(HttpStreamRegistry::new());
    let victim = registry.open_inbound_exchange().await;

    // Attacker writes into the victim's response-body handle.
    let attacker = attacker_host(registry.clone());
    let injected = attacker
        .http_stream_try_write(victim.guest_response_body, b"injected".to_vec())
        .await;

    assert_eq!(
        injected,
        Err(StreamError::InvalidHandle),
        "SECURITY: attacker injected into another invocation's response: {injected:?}"
    );
}

#[tokio::test]
async fn guest_cannot_close_another_invocations_stream() {
    let registry = Arc::new(HttpStreamRegistry::new());
    let victim = registry.open_inbound_exchange().await;

    // Attacker closes the victim's request-body handle (denial of service).
    let attacker = attacker_host(registry.clone());
    let closed = attacker.http_stream_close(victim.guest_request_body).await;

    assert_eq!(
        closed,
        Err(StreamError::InvalidHandle),
        "SECURITY: attacker closed another invocation's stream: {closed:?}"
    );
}
