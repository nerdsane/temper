//! ARN-207: process-global enumerable WASM stream handles must not be
//! authority-bearing capabilities.
//!
//! ServerState shares one `HttpStreamRegistry` across tenants. Handles
//! were sequential `u32` values, and guest-facing `ProductionWasmHost`
//! stream ops validated only that the raw handle existed. A malicious
//! guest can therefore enumerate another tenant's active handle and
//! read, inject, or close foreign request/response bodies.
//!
//! These tests document the secure contract. On unfixed code they fail
//! (RED) because the exploit currently succeeds; after the capability
//! fix they pass (GREEN).

use std::collections::BTreeMap;
use std::sync::Arc;

use temper_wasm::WasmHost;
use temper_wasm::host_trait::ProductionWasmHost;
use temper_wasm::http_stream::{HttpStreamRegistry, StreamError, StreamHandle};

/// Shared-registry layout mirrors `ServerState.http_stream_registry`.
fn shared_registry() -> Arc<HttpStreamRegistry> {
    Arc::new(HttpStreamRegistry::new())
}

#[tokio::test]
async fn cross_tenant_guest_cannot_read_foreign_request_body() {
    let registry = shared_registry();

    // Victim tenant opens an inbound exchange (HttpEndpoint path).
    let victim = registry.open_inbound_exchange().await;
    registry
        .write(victim.kernel_request_body, b"SECRET-TENANT-A-BODY".to_vec())
        .await
        .unwrap();

    // Attacker tenant: separate host, same process-global registry.
    let attacker = ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry.clone());

    let result = attacker.http_stream_read(victim.guest_request_body).await;

    assert!(
        matches!(result, Err(StreamError::InvalidHandle)),
        "cross-tenant stream read must be denied; got {result:?}"
    );

    // Victim body must remain intact for the legitimate owner path.
    let still_there = registry.read(victim.guest_request_body).await.unwrap();
    assert_eq!(&still_there, b"SECRET-TENANT-A-BODY");
}

#[tokio::test]
async fn cross_tenant_guest_cannot_write_foreign_response_body() {
    let registry = shared_registry();
    let victim = registry.open_inbound_exchange().await;

    let attacker = ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry.clone());
    let result = attacker
        .http_stream_try_write(victim.guest_response_body, b"injected-by-attacker".to_vec())
        .await;

    assert!(
        matches!(result, Err(StreamError::InvalidHandle)),
        "cross-tenant stream write must be denied; got {result:?}"
    );
}

#[tokio::test]
async fn cross_tenant_guest_cannot_close_foreign_handle() {
    let registry = shared_registry();
    let victim = registry.open_inbound_exchange().await;

    let attacker = ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry.clone());
    let result = attacker.http_stream_close(victim.guest_request_body).await;

    assert!(
        matches!(result, Err(StreamError::InvalidHandle)),
        "cross-tenant stream close must be denied; got {result:?}"
    );

    // Handle still usable after the denied close.
    registry
        .write(victim.kernel_request_body, b"still-open".to_vec())
        .await
        .unwrap();
    let chunk = registry.read(victim.guest_request_body).await.unwrap();
    assert_eq!(&chunk, b"still-open");
}

#[tokio::test]
async fn sequential_handle_enumeration_cannot_steal_body() {
    let registry = shared_registry();
    let victim = registry.open_inbound_exchange().await;
    registry
        .write(victim.kernel_request_body, b"secret-payload".to_vec())
        .await
        .unwrap();

    let attacker = ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry.clone());

    let mut stolen = false;
    for id in 1u32..=128 {
        match attacker.http_stream_read(StreamHandle(id)).await {
            Ok(chunk) if chunk == b"secret-payload" => {
                stolen = true;
                break;
            }
            Ok(_) | Err(_) => {}
        }
    }

    assert!(
        !stolen,
        "sequential handle enumeration must not yield a foreign stream body"
    );
}
