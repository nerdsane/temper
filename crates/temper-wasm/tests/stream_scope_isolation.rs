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
use temper_wasm::http_stream::{HttpStreamRegistry, StreamError, StreamScope};

/// Build an attacker host that shares the victim's registry — exactly how the
/// server hands every per-request host a clone of the one global registry —
/// under its own distinct invocation scope.
fn attacker_host(registry: Arc<HttpStreamRegistry>, scope: StreamScope) -> ProductionWasmHost {
    ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry, scope)
}

#[tokio::test]
async fn guest_cannot_read_another_invocations_request_body() {
    let registry = Arc::new(HttpStreamRegistry::new());

    // Victim invocation: kernel opens an inbound exchange and pumps the
    // victim's request body into it (as the axum body pump does).
    let victim_scope = registry.mint_scope().await;
    let victim = registry.open_inbound_exchange(victim_scope).await;
    registry
        .write(victim.kernel_request_body, b"victim-tenant-secret".to_vec())
        .await
        .unwrap();

    // Attacker guesses the victim's guest-facing read handle.
    let attacker = attacker_host(registry.clone(), registry.mint_scope().await);
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
    let victim_scope = registry.mint_scope().await;
    let victim = registry.open_inbound_exchange(victim_scope).await;

    // Attacker writes into the victim's response-body handle.
    let attacker = attacker_host(registry.clone(), registry.mint_scope().await);
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
    let victim_scope = registry.mint_scope().await;
    let victim = registry.open_inbound_exchange(victim_scope).await;

    // Attacker closes the victim's request-body handle (denial of service).
    let attacker = attacker_host(registry.clone(), registry.mint_scope().await);
    let closed = attacker.http_stream_close(victim.guest_request_body).await;

    assert_eq!(
        closed,
        Err(StreamError::InvalidHandle),
        "SECURITY: attacker closed another invocation's stream: {closed:?}"
    );
}

/// The legitimate owner is unaffected: its own guest ops still succeed.
#[tokio::test]
async fn owner_can_still_operate_its_own_stream() {
    let registry = Arc::new(HttpStreamRegistry::new());
    let scope = registry.mint_scope().await;
    let exchange = registry.open_inbound_exchange(scope).await;
    registry
        .write(exchange.kernel_request_body, b"legit-body".to_vec())
        .await
        .unwrap();

    let owner = ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry.clone(), scope);
    let body = owner
        .http_stream_read(exchange.guest_request_body)
        .await
        .expect("owner reads its own request body");
    assert_eq!(body, b"legit-body");
}
