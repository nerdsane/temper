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
    let victim = registry.open_inbound_exchange().await.unwrap();
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
    let victim = registry.open_inbound_exchange().await.unwrap();

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
    let victim = registry.open_inbound_exchange().await.unwrap();

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
    let victim = registry.open_inbound_exchange().await.unwrap();
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

#[tokio::test]
async fn legitimate_owner_can_use_granted_inbound_handles() {
    let registry = shared_registry();
    let exchange = registry.open_inbound_exchange().await.unwrap();
    registry
        .write(exchange.kernel_request_body, b"hello-owner".to_vec())
        .await
        .unwrap();

    let owner = ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry.clone());
    owner
        .grant_stream_handles([exchange.guest_request_body, exchange.guest_response_body])
        .unwrap();

    let body = owner
        .http_stream_read(exchange.guest_request_body)
        .await
        .expect("owner must read granted request body");
    assert_eq!(&body, b"hello-owner");

    let n = owner
        .http_stream_try_write(exchange.guest_response_body, b"reply".to_vec())
        .await
        .expect("owner must write granted response body");
    assert_eq!(n, 5);

    let drained = registry.read(exchange.kernel_response_body).await.unwrap();
    assert_eq!(&drained, b"reply");
}

#[tokio::test]
async fn guest_cannot_operate_on_kernel_side_handles_even_when_guessed() {
    let registry = shared_registry();
    let exchange = registry.open_inbound_exchange().await.unwrap();

    let owner = ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry.clone());
    // Grant only guest ends — mirrors HttpEndpoint dispatcher.
    owner
        .grant_stream_handles([exchange.guest_request_body, exchange.guest_response_body])
        .unwrap();

    let kernel_read = owner.http_stream_read(exchange.kernel_request_body).await;
    assert!(
        matches!(kernel_read, Err(StreamError::InvalidHandle)),
        "guest must not read kernel-side handle; got {kernel_read:?}"
    );

    let kernel_write = owner
        .http_stream_try_write(exchange.kernel_response_body, b"nope".to_vec())
        .await;
    assert!(
        matches!(kernel_write, Err(StreamError::InvalidHandle)),
        "guest must not write kernel-side handle; got {kernel_write:?}"
    );
}

#[tokio::test]
async fn cross_tenant_guest_cannot_send_or_await_foreign_response_head() {
    let registry = shared_registry();
    let victim = registry.open_inbound_exchange().await.unwrap();

    let attacker = ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry.clone());

    let send = attacker
        .http_stream_send_response_head(
            victim.guest_response_body,
            temper_wasm::http_stream::HttpResponseHead {
                status: 200,
                headers: vec![],
            },
        )
        .await;
    assert!(
        matches!(send, Err(StreamError::InvalidHandle)),
        "cross-tenant send_response_head must be denied; got {send:?}"
    );

    // Outbound head await on a foreign inbound response handle is also denied.
    let await_head = attacker
        .http_stream_response_head(victim.guest_response_body)
        .await;
    assert!(
        await_head.is_err(),
        "cross-tenant response_head await must be denied; got {await_head:?}"
    );
}

#[tokio::test]
async fn close_revokes_grant_so_handle_cannot_be_reused() {
    let registry = shared_registry();
    let exchange = registry.open_inbound_exchange().await.unwrap();
    let owner = ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry.clone());
    owner
        .grant_stream_handles([exchange.guest_request_body, exchange.guest_response_body])
        .unwrap();

    owner
        .http_stream_close(exchange.guest_response_body)
        .await
        .unwrap();
    assert!(
        !owner.has_stream_grant(exchange.guest_response_body),
        "successful close must revoke the grant"
    );
    let reclose = owner.http_stream_close(exchange.guest_response_body).await;
    assert!(
        matches!(reclose, Err(StreamError::InvalidHandle)),
        "revoked grant must deny further ops; got {reclose:?}"
    );
}

#[tokio::test]
async fn concurrent_tenants_cannot_cross_read_shared_registry() {
    let registry = shared_registry();

    let a = registry.open_inbound_exchange().await.unwrap();
    let b = registry.open_inbound_exchange().await.unwrap();
    registry
        .write(a.kernel_request_body, b"tenant-a".to_vec())
        .await
        .unwrap();
    registry
        .write(b.kernel_request_body, b"tenant-b".to_vec())
        .await
        .unwrap();

    let host_a = ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry.clone());
    host_a
        .grant_stream_handles([a.guest_request_body, a.guest_response_body])
        .unwrap();
    let host_b = ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry.clone());
    host_b
        .grant_stream_handles([b.guest_request_body, b.guest_response_body])
        .unwrap();

    // Each owner reads only its own body.
    assert_eq!(
        host_a.http_stream_read(a.guest_request_body).await.unwrap(),
        b"tenant-a"
    );
    assert_eq!(
        host_b.http_stream_read(b.guest_request_body).await.unwrap(),
        b"tenant-b"
    );

    // Cross-tenant reads denied.
    assert!(matches!(
        host_a.http_stream_read(b.guest_request_body).await,
        Err(StreamError::InvalidHandle)
    ));
    assert!(matches!(
        host_b.http_stream_read(a.guest_request_body).await,
        Err(StreamError::InvalidHandle)
    ));
}

#[tokio::test]
async fn close_granted_streams_clears_all_grants() {
    let registry = shared_registry();
    let exchange = registry.open_inbound_exchange().await.unwrap();
    let owner = ProductionWasmHost::with_shared_streams(BTreeMap::new(), registry.clone());
    owner
        .grant_stream_handles([exchange.guest_request_body, exchange.guest_response_body])
        .unwrap();
    owner.close_granted_streams().await;
    assert!(!owner.has_stream_grant(exchange.guest_request_body));
    assert!(!owner.has_stream_grant(exchange.guest_response_body));
    let read = owner.http_stream_read(exchange.guest_request_body).await;
    assert!(matches!(read, Err(StreamError::InvalidHandle)));
}
