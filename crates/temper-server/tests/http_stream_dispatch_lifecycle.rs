//! Deterministic coverage for the HttpEndpoint dispatch scope lifecycle
//! (ARN-207 / ADR-0156), mirroring the wiring in `router.rs`:
//!
//! * every inbound dispatch mints a fresh `StreamScope` and opens its
//!   exchange under it, so two concurrent requests are isolated;
//! * the scope is reclaimed when the exchange ends — on the success
//!   drain path and on the guest-timeout path alike — leaving no
//!   entries resident in the shared registry.
//!
//! These reproduce the exact sequence the router performs against
//! `ServerState.http_stream_registry` without needing a live guest, and
//! they fail on the pre-fix behavior (one global registry, no scope,
//! no scope-bound cleanup).

use std::sync::Arc;

use temper_wasm::http_stream::{
    HttpResponseHead, HttpStreamRegistry, ScopeCleanupGuard, StreamError,
};

/// Two concurrent inbound dispatches on one shared registry (as
/// `ServerState` holds) each get a distinct scope; neither can reach the
/// other's handles.
#[tokio::test]
async fn concurrent_dispatches_are_scope_isolated() {
    let registry = Arc::new(HttpStreamRegistry::new());

    let scope_a = registry.mint_scope().await;
    let ex_a = registry.open_inbound_exchange(scope_a).await;
    let scope_b = registry.mint_scope().await;
    let ex_b = registry.open_inbound_exchange(scope_b).await;

    assert_ne!(scope_a, scope_b);

    // Request A's guest handle is not usable under request B's scope.
    assert_eq!(
        registry
            .read_as_guest(scope_b, ex_a.guest_request_body)
            .await,
        Err(StreamError::InvalidHandle)
    );
    // ...and vice versa.
    assert_eq!(
        registry
            .try_write_as_guest(scope_a, ex_b.guest_response_body, b"x".to_vec())
            .await,
        Err(StreamError::InvalidHandle)
    );
}

/// Success path: after the guest completes and the kernel drains the
/// response, the router's cleanup guard closes the scope, leaving the
/// shared registry empty.
#[tokio::test]
async fn scope_reclaimed_after_successful_drain() {
    let registry = Arc::new(HttpStreamRegistry::new());
    let scope = registry.mint_scope().await;
    let ex = registry.open_inbound_exchange(scope).await;

    // Kernel pumps request body; guest reads it and produces a response.
    registry
        .write(ex.kernel_request_body, b"req".to_vec())
        .await
        .unwrap();
    registry.close(ex.kernel_request_body).await.unwrap();
    let _ = registry.read(ex.guest_request_body).await.unwrap();

    registry
        .submit_inbound_response_head_as_guest(
            scope,
            ex.guest_response_body,
            HttpResponseHead {
                status: 200,
                headers: vec![],
            },
        )
        .await
        .unwrap();
    registry
        .try_write_as_guest(scope, ex.guest_response_body, b"resp".to_vec())
        .await
        .unwrap();
    registry
        .close_as_guest(scope, ex.guest_response_body)
        .await
        .unwrap();
    let _ = registry
        .await_inbound_response_head(ex.kernel_head_receiver_slot)
        .await
        .unwrap();
    let _ = registry.read(ex.kernel_response_body).await.unwrap();

    assert!(registry.handle_count().await > 0);
    registry.close_scope(scope).await;
    assert_eq!(registry.handle_count().await, 0);
}

/// Timeout path: a guest that never submits its response head does not
/// leak registry entries — the router's timeout branch closes the scope.
#[tokio::test]
async fn scope_reclaimed_on_guest_timeout() {
    let registry = Arc::new(HttpStreamRegistry::new());
    let scope = registry.mint_scope().await;
    let ex = registry.open_inbound_exchange(scope).await;

    // Kernel wrote the request body; the guest wedged before responding.
    registry
        .write(ex.kernel_request_body, b"req".to_vec())
        .await
        .unwrap();
    assert_eq!(registry.handle_count().await, 4);

    // Router timeout branch: reclaim the scope.
    registry.close_scope(scope).await;
    assert_eq!(registry.handle_count().await, 0);

    // Every handle from the timed-out request is now inert.
    assert_eq!(
        registry.read_as_guest(scope, ex.guest_request_body).await,
        Err(StreamError::InvalidHandle)
    );
}

/// Cancellation path: the router holds a `ScopeCleanupGuard` from the moment it
/// opens the exchange, so if the dispatch handler future is dropped mid-request
/// (client disconnect while the guest is still running) the scope is reclaimed
/// on drop rather than leaking in the shared registry.
#[tokio::test]
async fn scope_reclaimed_when_dispatch_future_dropped_midflight() {
    let registry = Arc::new(HttpStreamRegistry::new());
    let scope = registry.mint_scope().await;
    let ex = registry.open_inbound_exchange(scope).await;
    registry
        .write(ex.kernel_request_body, b"partial".to_vec())
        .await
        .unwrap();
    assert_eq!(registry.handle_count().await, 4);

    // The guest is mid-execution and nothing on the normal control-flow
    // paths has run; dropping the guard simulates the handler future being
    // cancelled by a client disconnect.
    {
        let _guard = ScopeCleanupGuard::new(registry.clone(), scope);
    }

    assert_eq!(registry.handle_count().await, 0);
}
