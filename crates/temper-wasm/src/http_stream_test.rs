//! Unit tests for the invocation-scoped HTTP stream registry.
//! Split out of `http_stream.rs` to keep that file readable (ADR-0156).

use super::*;

#[tokio::test]
async fn create_pair_returns_distinct_ids() {
    let reg = HttpStreamRegistry::new();
    let (w, r) = reg.create_pair().await;
    assert_ne!(w.0, r.0);
    assert_eq!(reg.handle_count().await, 2);
}

#[tokio::test]
async fn write_then_read_roundtrips_chunk() {
    let reg = HttpStreamRegistry::new();
    let (w, r) = reg.create_pair().await;
    let n = reg.write(w, b"hello".to_vec()).await.unwrap();
    assert_eq!(n, 5);
    let chunk = reg.read(r).await.unwrap();
    assert_eq!(&chunk, b"hello");
}

#[tokio::test]
async fn bounded_read_splits_oversized_chunk_and_preserves_order() {
    let reg = HttpStreamRegistry::new();
    let (w, r) = reg.create_pair().await;
    reg.write(w, b"abcdefghij".to_vec()).await.unwrap();
    reg.write(w, b"next".to_vec()).await.unwrap();

    let first = reg.read_bounded(r, 4).await.unwrap();
    let second = reg.read_bounded(r, 4).await.unwrap();
    let third = reg.read_bounded(r, 4).await.unwrap();
    let fourth = reg.read_bounded(r, 4).await.unwrap();

    assert_eq!(&first, b"abcd");
    assert_eq!(&second, b"efgh");
    assert_eq!(&third, b"ij");
    assert_eq!(&fourth, b"next");
}

#[tokio::test]
async fn write_into_receiver_handle_is_invalid() {
    let reg = HttpStreamRegistry::new();
    let (_w, r) = reg.create_pair().await;
    let err = reg.write(r, b"hi".to_vec()).await.unwrap_err();
    assert_eq!(err, StreamError::InvalidHandle);
}

#[tokio::test]
async fn read_from_sender_handle_is_invalid() {
    let reg = HttpStreamRegistry::new();
    let (w, _r) = reg.create_pair().await;
    let err = reg.read(w).await.unwrap_err();
    assert_eq!(err, StreamError::InvalidHandle);
}

#[tokio::test]
async fn try_write_returns_wouldblock_when_full() {
    let reg = HttpStreamRegistry::new();
    let (w, _r) = reg.create_pair().await;
    // Fill the channel to capacity.
    for _ in 0..STREAM_CHANNEL_CAPACITY {
        reg.try_write(w, vec![0u8; 8]).await.unwrap();
    }
    // Next try_write should WouldBlock.
    let err = reg.try_write(w, vec![0u8; 8]).await.unwrap_err();
    assert_eq!(err, StreamError::WouldBlock);
}

#[tokio::test]
async fn close_sender_causes_receiver_eof() {
    let reg = HttpStreamRegistry::new();
    let (w, r) = reg.create_pair().await;
    reg.write(w, b"first".to_vec()).await.unwrap();
    reg.close(w).await.unwrap();
    // Drain buffered chunk then EOF.
    let first = reg.read(r).await.unwrap();
    assert_eq!(&first, b"first");
    let eof = reg.read(r).await.unwrap();
    assert!(eof.is_empty(), "expected EOF, got {:?}", eof);
}

#[tokio::test]
async fn write_after_receiver_close_returns_closed() {
    let reg = HttpStreamRegistry::new();
    let (w, r) = reg.create_pair().await;
    reg.close(r).await.unwrap();
    let err = reg.write(w, b"x".to_vec()).await.unwrap_err();
    assert_eq!(err, StreamError::Closed);
}

#[tokio::test]
async fn close_unknown_handle_errs() {
    let reg = HttpStreamRegistry::new();
    let err = reg.close(StreamHandle(9999)).await.unwrap_err();
    assert_eq!(err, StreamError::InvalidHandle);
}

#[tokio::test]
async fn inbound_exchange_roundtrips_head_and_body() {
    let reg = HttpStreamRegistry::new();
    let exchange = reg.open_inbound_exchange(StreamScope::PRIVATE).await;

    // Kernel pushes request body.
    reg.write(
        exchange.kernel_request_body,
        b"git-upload-pack-request-body".to_vec(),
    )
    .await
    .unwrap();
    reg.close(exchange.kernel_request_body).await.unwrap();

    // Guest reads request, produces response body + head.
    let chunk = reg.read(exchange.guest_request_body).await.unwrap();
    assert_eq!(&chunk, b"git-upload-pack-request-body");
    let eof = reg.read(exchange.guest_request_body).await.unwrap();
    assert!(eof.is_empty());

    // Guest submits head.
    let head = HttpResponseHead {
        status: 200,
        headers: vec![(
            "content-type".into(),
            "application/x-git-upload-pack-result".into(),
        )],
    };
    reg.submit_inbound_response_head(exchange.guest_response_body, head.clone())
        .await
        .unwrap();

    // Guest writes response body and closes.
    reg.write(exchange.guest_response_body, b"packfile-bytes".to_vec())
        .await
        .unwrap();
    reg.close(exchange.guest_response_body).await.unwrap();

    // Kernel awaits head, then drains response body.
    let kernel_head = reg
        .await_inbound_response_head(exchange.kernel_head_receiver_slot)
        .await
        .unwrap();
    assert_eq!(kernel_head.status, head.status);
    assert_eq!(kernel_head.headers, head.headers);
    let body_chunk = reg.read(exchange.kernel_response_body).await.unwrap();
    assert_eq!(&body_chunk, b"packfile-bytes");
    let eof2 = reg.read(exchange.kernel_response_body).await.unwrap();
    assert!(eof2.is_empty());
}

#[tokio::test]
async fn inbound_submit_head_unknown_handle() {
    let reg = HttpStreamRegistry::new();
    let head = HttpResponseHead::default();
    let err = reg
        .submit_inbound_response_head(StreamHandle(9999), head)
        .await
        .unwrap_err();
    assert_eq!(err, StreamError::InvalidHandle);
}

#[tokio::test]
async fn handle_ids_monotonic_within_registry() {
    let reg = HttpStreamRegistry::new();
    let (w1, r1) = reg.create_pair().await;
    let (w2, r2) = reg.create_pair().await;
    assert!(w1.0 < r1.0);
    assert!(r1.0 < w2.0);
    assert!(w2.0 < r2.0);
}

#[tokio::test]
async fn minted_scopes_are_distinct_and_skip_private() {
    let reg = HttpStreamRegistry::new();
    let a = reg.mint_scope().await;
    let b = reg.mint_scope().await;
    assert_ne!(a, b);
    assert_ne!(a, StreamScope::PRIVATE);
    assert_ne!(b, StreamScope::PRIVATE);
}

#[tokio::test]
async fn guest_ops_denied_across_scopes() {
    // Two invocations share one registry, as ServerState does.
    let reg = HttpStreamRegistry::new();
    let victim = reg.mint_scope().await;
    let attacker = reg.mint_scope().await;

    let vex = reg.open_inbound_exchange(victim).await;
    reg.write(vex.kernel_request_body, b"victim-secret".to_vec())
        .await
        .unwrap();

    // Attacker presents its own scope against the victim's handles.
    assert_eq!(
        reg.read_as_guest(attacker, vex.guest_request_body).await,
        Err(StreamError::InvalidHandle)
    );
    assert_eq!(
        reg.try_write_as_guest(attacker, vex.guest_response_body, b"x".to_vec())
            .await,
        Err(StreamError::InvalidHandle)
    );
    assert_eq!(
        reg.close_as_guest(attacker, vex.guest_request_body).await,
        Err(StreamError::InvalidHandle)
    );

    // The legitimate owner still reads its own request body.
    assert_eq!(
        reg.read_as_guest(victim, vex.guest_request_body)
            .await
            .unwrap(),
        b"victim-secret"
    );
}

#[tokio::test]
async fn guest_cannot_touch_kernel_facing_handle_in_own_scope() {
    let reg = HttpStreamRegistry::new();
    let scope = reg.mint_scope().await;
    let ex = reg.open_inbound_exchange(scope).await;
    // kernel_response_body belongs to the same scope but is not
    // guest-facing: the guest must not read the kernel's side.
    assert_eq!(
        reg.read_as_guest(scope, ex.kernel_response_body).await,
        Err(StreamError::InvalidHandle)
    );
    assert_eq!(
        reg.try_write_as_guest(scope, ex.kernel_request_body, b"x".to_vec())
            .await,
        Err(StreamError::InvalidHandle)
    );
}

#[tokio::test]
async fn close_scope_reclaims_all_handles() {
    let reg = HttpStreamRegistry::new();
    let scope = reg.mint_scope().await;
    let _ex = reg.open_inbound_exchange(scope).await;
    assert_eq!(reg.handle_count().await, 4);
    reg.close_scope(scope).await;
    assert_eq!(reg.handle_count().await, 0);
    // Idempotent.
    reg.close_scope(scope).await;
    assert_eq!(reg.handle_count().await, 0);
}

#[tokio::test]
async fn concurrent_outbound_streams_are_bounded_per_scope() {
    let reg = HttpStreamRegistry::new();
    let scope = reg.mint_scope().await;
    let mut open = Vec::new();
    for _ in 0..MAX_OUTBOUND_STREAMS_PER_SCOPE {
        open.push(reg.open_outbound_exchange(scope).await.unwrap());
    }
    // One more concurrent exchange is refused.
    let over = reg.open_outbound_exchange(scope).await;
    assert!(matches!(over, Err(StreamError::Aborted(_))));
    // A different invocation is unaffected by the first's budget.
    let other = reg.mint_scope().await;
    assert!(reg.open_outbound_exchange(other).await.is_ok());
}

#[tokio::test]
async fn sequential_outbound_streams_do_not_exhaust_budget() {
    // A guest that closes each exchange before opening the next can
    // make far more than the concurrency limit of outbound calls.
    let reg = HttpStreamRegistry::new();
    let scope = reg.mint_scope().await;
    for _ in 0..(MAX_OUTBOUND_STREAMS_PER_SCOPE * 3) {
        let ex = reg
            .open_outbound_exchange(scope)
            .await
            .expect("sequential outbound call must be allowed");
        // Closing the guest response-body releases the concurrency slot.
        reg.close_as_guest(scope, ex.guest_response_body)
            .await
            .unwrap();
    }
}
