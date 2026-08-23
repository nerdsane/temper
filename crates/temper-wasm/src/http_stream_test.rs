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
async fn completed_outbound_exchanges_do_not_exhaust_budget() {
    // A guest whose bridge tasks complete before opening the next exchange can
    // make far more than the concurrency limit of outbound calls. Bridge
    // completion is modelled here by `release_outbound_exchange`, which the real
    // bridge calls when its socket closes.
    let reg = HttpStreamRegistry::new();
    let scope = reg.mint_scope().await;
    for _ in 0..(MAX_OUTBOUND_STREAMS_PER_SCOPE * 3) {
        let ex = reg
            .open_outbound_exchange(scope)
            .await
            .expect("sequential outbound call must be allowed");
        // Fully done: bridge completes AND the guest drains/closes its response.
        reg.release_outbound_exchange(
            scope,
            ex.guest_response_body,
            ex.guest_request_body,
            ex.bridge_request_body,
            ex.bridge_response_body,
        )
        .await;
        reg.close_as_guest(scope, ex.guest_response_body)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn undrained_response_handles_still_count_against_the_cap() {
    // ARN-207: an exchange occupies a slot until BOTH its bridge completes AND
    // its guest response handle is closed. A guest that lets bridges finish but
    // never drains their responses must not accumulate uncounted response
    // buffers past the cap — closing the socket half is not enough.
    let reg = HttpStreamRegistry::new();
    let scope = reg.mint_scope().await;

    let mut exchanges = Vec::new();
    for _ in 0..MAX_OUTBOUND_STREAMS_PER_SCOPE {
        let ex = reg.open_outbound_exchange(scope).await.expect("within cap");
        // Bridge completes, but the guest never closes/drains the response.
        reg.release_outbound_exchange(
            scope,
            ex.guest_response_body,
            ex.guest_request_body,
            ex.bridge_request_body,
            ex.bridge_response_body,
        )
        .await;
        exchanges.push(ex);
    }

    // Every bridge is done, yet the response buffers are all still resident, so
    // the cap is full.
    let over = reg.open_outbound_exchange(scope).await;
    assert!(
        matches!(over, Err(StreamError::Aborted(_))),
        "bridge completion alone must not free a slot while the response is undrained"
    );

    // Draining one (closing the guest response handle) frees exactly one slot.
    reg.close_as_guest(scope, exchanges[0].guest_response_body)
        .await
        .unwrap();
    assert!(
        reg.open_outbound_exchange(scope).await.is_ok(),
        "closing a drained response must free its slot"
    );
}

#[tokio::test]
async fn closing_the_guest_handle_does_not_release_the_outbound_slot() {
    // ARN-207: the concurrency slot represents a live bridge task — a real
    // socket and its buffered request bytes — not the guest's read handle. A
    // guest that loops `begin_outbound -> close(response)` without its bridges
    // completing must NOT be able to exceed the cap. The slot frees only on
    // `release_outbound_exchange` (bridge completion).
    let reg = HttpStreamRegistry::new();
    let scope = reg.mint_scope().await;

    let mut exchanges = Vec::new();
    for _ in 0..MAX_OUTBOUND_STREAMS_PER_SCOPE {
        let ex = reg.open_outbound_exchange(scope).await.expect("within cap");
        // Guest closes its read handle immediately — but the bridge is "still
        // running" (release not called), so the slot is NOT freed.
        reg.close_as_guest(scope, ex.guest_response_body)
            .await
            .unwrap();
        exchanges.push(ex);
    }

    // The cap is full despite every guest handle being closed, because the
    // sockets are still live.
    let over = reg.open_outbound_exchange(scope).await;
    assert!(
        matches!(over, Err(StreamError::Aborted(_))),
        "closing guest handles must not free slots while bridges are live"
    );

    // When a bridge completes and the guest has already closed (both done), its
    // slot frees and a new exchange is admitted.
    let done = &exchanges[0];
    reg.release_outbound_exchange(
        scope,
        done.guest_response_body,
        done.guest_request_body,
        done.bridge_request_body,
        done.bridge_response_body,
    )
    .await;
    assert!(
        reg.open_outbound_exchange(scope).await.is_ok(),
        "a fully-done exchange must free its slot"
    );
}

#[tokio::test]
async fn release_reclaims_the_request_channel_and_frees_the_slot_on_close() {
    // ARN-207: release frees the request channel (request done) but spares the
    // response handle (guest draining). This pins both — the request handles are
    // gone after release, the response handle survives, and the slot frees when
    // the guest then closes.
    let reg = HttpStreamRegistry::new();
    let scope = reg.mint_scope().await;
    let ex = reg.open_outbound_exchange(scope).await.expect("open");

    reg.release_outbound_exchange(
        scope,
        ex.guest_response_body,
        ex.guest_request_body,
        ex.bridge_request_body,
        ex.bridge_response_body,
    )
    .await;

    // Request channel reclaimed: guest write to its request handle is refused.
    assert_eq!(
        reg.write(ex.guest_request_body, b"x".to_vec()).await,
        Err(StreamError::InvalidHandle),
        "release must reclaim the request write handle"
    );
    // Response handle spared: closing it succeeds (Ok), rather than failing
    // InvalidHandle as it would if release had removed it.
    assert_eq!(
        reg.close_as_guest(scope, ex.guest_response_body).await,
        Ok(()),
        "release must spare the response handle for the guest to drain"
    );
}

#[tokio::test]
async fn close_scope_aborts_detached_outbound_bridges() {
    // ARN-358: close_scope must cancel a scope's live outbound bridge tasks so
    // their sockets die at request end, not at the reqwest timeout. Modelled with
    // a task that would run "forever" (a stalled upstream), registered against a
    // real open exchange; after close_scope it must be aborted.
    let reg = Arc::new(HttpStreamRegistry::new());
    let scope = reg.mint_scope().await;
    let ex = reg.open_outbound_exchange(scope).await.expect("open");

    let ran_to_completion = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = ran_to_completion.clone();
    let join = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    reg.register_outbound_bridge(scope, ex.guest_response_body, join.abort_handle())
        .await;

    reg.close_scope(scope).await;

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), join)
        .await
        .expect(
            "close_scope must abort the bridge — awaiting it timed out, so it was not cancelled",
        );
    assert!(
        outcome.is_err() && outcome.unwrap_err().is_cancelled(),
        "close_scope must abort the scope's live bridge tasks"
    );
    assert!(
        !ran_to_completion.load(std::sync::atomic::Ordering::SeqCst),
        "the aborted bridge must not have run to completion"
    );
}

#[tokio::test]
async fn register_after_close_scope_aborts_the_bridge_and_does_not_reopen_the_scope() {
    // ARN-207 review (High): a host call can outlive close_scope (block-in-place
    // after abort, or a disconnect racing the drain). Registering a bridge after
    // the scope is closed must abort it immediately, not resurrect the scope.
    let reg = Arc::new(HttpStreamRegistry::new());
    let scope = reg.mint_scope().await;
    let ex = reg.open_outbound_exchange(scope).await.expect("open");

    // Cleanup runs first.
    reg.close_scope(scope).await;

    let join = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    });
    // Registration arrives late — the scope is already closed.
    reg.register_outbound_bridge(scope, ex.guest_response_body, join.abort_handle())
        .await;

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), join)
        .await
        .expect("a bridge registered under a closed scope must be aborted, not left running");
    assert!(
        outcome.is_err() && outcome.unwrap_err().is_cancelled(),
        "registration under a closed scope must abort the bridge"
    );

    // And the closed scope cannot be reopened.
    assert!(
        matches!(
            reg.open_outbound_exchange(scope).await,
            Err(StreamError::Aborted(_))
        ),
        "a closed scope must not admit new outbound exchanges"
    );
}

#[tokio::test]
async fn completed_bridges_do_not_accumulate_abort_handles() {
    // ARN-207 review (Medium): the live-bridge map tracks live bridges only, not
    // every call ever made. Many sequential completed exchanges must not grow it.
    let reg = Arc::new(HttpStreamRegistry::new());
    let scope = reg.mint_scope().await;

    for _ in 0..(MAX_OUTBOUND_STREAMS_PER_SCOPE * 3) {
        let ex = reg.open_outbound_exchange(scope).await.expect("open");
        // Register a trivially-complete bridge, then release (bridge completion).
        let join = tokio::spawn(async {});
        reg.register_outbound_bridge(scope, ex.guest_response_body, join.abort_handle())
            .await;
        let _ = join.await;
        reg.release_outbound_exchange(
            scope,
            ex.guest_response_body,
            ex.guest_request_body,
            ex.bridge_request_body,
            ex.bridge_response_body,
        )
        .await;
        reg.close_as_guest(scope, ex.guest_response_body)
            .await
            .unwrap();
    }

    assert!(
        reg.live_outbound_bridge_count(scope).await <= MAX_OUTBOUND_STREAMS_PER_SCOPE,
        "completed bridges must be dropped from the live map, not accumulated"
    );
}

#[tokio::test]
async fn closing_the_bridge_response_writer_gives_the_guest_eof() {
    // ARN-207 review (Medium): the panic path now closes `bridge_resp` (the
    // response writer) explicitly, because the normal end-of-bridge close is
    // skipped on panic. Without an EOF signal the guest blocks forever waiting to
    // drain. This pins the property the panic-path close relies on: closing the
    // writer makes the guest's read return EOF rather than hang.
    let reg = HttpStreamRegistry::new();
    let scope = reg.mint_scope().await;
    let ex = reg.open_outbound_exchange(scope).await.expect("open");

    // No response was ever written. Close the bridge-side writer (what the panic
    // path does), then the guest read must see EOF (empty), not hang.
    reg.close(ex.bridge_response_body).await.unwrap();
    let chunk = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reg.read_as_guest(scope, ex.guest_response_body),
    )
    .await
    .expect("guest read must not hang once the bridge writer is closed")
    .expect("read of a closed-writer response must succeed with EOF");
    assert!(
        chunk.is_empty(),
        "closing the response writer must give the guest EOF, got {} bytes",
        chunk.len()
    );
}

#[tokio::test]
async fn live_scope_set_is_bounded_by_concurrency_not_total_requests() {
    // ARN-207 review: tracking *live* scopes (insert at mint, remove at close)
    // rather than tombstoning closed ones means the set is bounded by concurrent
    // invocations, not total requests — no unbounded growth, no eviction hazard
    // that could let a straggler reopen a forgotten scope.
    let reg = HttpStreamRegistry::new();
    // Baseline: the always-live PRIVATE scope for this (private) registry.
    let baseline = reg.live_scope_count().await;
    for _ in 0..10_000 {
        let scope = reg.mint_scope().await;
        reg.close_scope(scope).await;
        assert_eq!(
            reg.live_scope_count().await,
            baseline,
            "a closed scope must leave the live set immediately — no accumulation"
        );
    }

    // Concurrent (unclosed) scopes are the only thing that grows it.
    let mut open = Vec::new();
    for _ in 0..5 {
        open.push(reg.mint_scope().await);
    }
    assert_eq!(reg.live_scope_count().await, baseline + 5);
    for scope in open {
        reg.close_scope(scope).await;
    }
    assert_eq!(reg.live_scope_count().await, baseline);
}

#[tokio::test]
async fn a_fast_bridge_that_released_before_guest_close_leaves_no_stale_abort_handle() {
    // ARN-207 review P3: if the bridge completes (release) before the guest closes
    // its response, and a late registration re-admitted an abort handle, closing
    // the guest response must drop it so it cannot accumulate to close_scope.
    let reg = HttpStreamRegistry::new();
    let scope = reg.mint_scope().await;
    let ex = reg.open_outbound_exchange(scope).await.expect("open");

    // Bridge completes first (guest hasn't closed): slot stays, marked done.
    reg.release_outbound_exchange(
        scope,
        ex.guest_response_body,
        ex.guest_request_body,
        ex.bridge_request_body,
        ex.bridge_response_body,
    )
    .await;
    // A late registration for the (now-finished) bridge is re-admitted by the
    // slot-present check.
    let join = tokio::spawn(async {});
    let abort = join.abort_handle();
    let _ = join.await;
    reg.register_outbound_bridge(scope, ex.guest_response_body, abort)
        .await;

    // Guest closes: the slot frees AND the stale abort handle is dropped.
    reg.close_as_guest(scope, ex.guest_response_body)
        .await
        .unwrap();
    assert_eq!(
        reg.live_outbound_bridge_count(scope).await,
        0,
        "closing the response must drop the stale abort handle"
    );
}

#[tokio::test]
async fn private_scope_is_always_live_for_unshared_registries() {
    // ARN-207: PRIVATE is the fixed scope for unshared/private-registry hosts. It
    // is never minted or close_scoped, so the live-scope gate must treat it as
    // always live — otherwise every outbound call on a private registry (e.g.
    // ProductionWasmHost::new()) is refused. Regression guard for the live-scope
    // change.
    let reg = HttpStreamRegistry::new();
    let ex = reg
        .open_outbound_exchange(StreamScope::PRIVATE)
        .await
        .expect("PRIVATE scope must admit outbound exchanges on a private registry");
    // And a bridge registers under it without being aborted.
    let join = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    });
    let abort = join.abort_handle();
    reg.register_outbound_bridge(StreamScope::PRIVATE, ex.guest_response_body, abort)
        .await;
    assert_eq!(
        reg.live_outbound_bridge_count(StreamScope::PRIVATE).await,
        1,
        "a bridge under PRIVATE must be tracked, not aborted as a dead scope"
    );
    join.abort();
}
