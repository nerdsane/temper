//! Production-boundary regressions for process-local committed-operation receipts.
#[path = "committed_operation_completion/fixture.rs"]
mod fixture;
use fixture::*;
use futures_util::poll;
use std::sync::atomic::Ordering;
use std::time::Duration;
use temper_runtime::scheduler::install_deterministic_context;

#[tokio::test(start_paused = true)]
async fn credentialed_caller_cancellation_does_not_release_surviving_execution() {
    let (_guard, _, _) = install_deterministic_context(51970);
    let (state, store, adapter) = credential_fixture().await;
    let mut first = Box::pin(run(&state, "Work", true, true));
    tokio::select! { biased;
        reply = &mut first => panic!("returned before adapter: {reply:?}"),
        _ = adapter.started.notified() => {}
    }
    let mut retry = Box::pin(run(&state, "Work", true, true));
    pending(&mut retry).await;
    let token_hash = adapter.hashes.lock().unwrap()[0].clone();
    assert_eq!(credential_status(&state, &token_hash).await, "Active");
    drop(first);
    pending(&mut retry).await;
    assert_eq!(
        adapter.calls.load(Ordering::SeqCst),
        1,
        "caller detachment started a second adapter while the original credential was active"
    );
    adapter.finish.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(1), retry)
        .await
        .unwrap();
    assert!(result.success, "{result:?}");
    assert_eq!(credential_status(&state, &token_hash).await, "Revoked");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(action_count(&store, "Work", "Start"), 1);
    assert_eq!(
        action_count_id(&store, "AgentCredential", &token_hash, "Revoke"),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn cancelled_caller_does_not_abandon_unfinished_reactions() {
    let (_guard, _, _) = install_deterministic_context(51971);
    let (state, store, barrier) = reaction_fixture(false).await;
    let mut first = Box::pin(run(&state, "Source", false, true));
    tokio::select! { biased;
        result = &mut first => panic!("source returned before dependent commit: {result:?}"),
        _ = barrier.entered.notified() => {}
    }
    let mut retry = Box::pin(run(&state, "Source", false, true));
    pending(&mut retry).await;
    drop(first);
    pending(&mut retry).await;
    assert_eq!(action_count(&store, "Child", "Start"), 0);
    barrier.release.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(1), retry)
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(action_count(&store, "Source", "Start"), 1);
    assert_eq!(action_count(&store, "Child", "Start"), 1);
    assert_eq!(action_count(&store, "Leaf", "Start"), 1);
    let replay = run(&state, "Source", false, true).await;
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    assert_eq!(action_count(&store, "Leaf", "Start"), 1);
}

async fn pending<F: std::future::Future>(future: &mut std::pin::Pin<Box<F>>) {
    for _ in 0..32 {
        assert!(
            poll!(future.as_mut()).is_pending(),
            "retry returned before its requested completion milestone"
        );
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn retry_after_cancelled_reaction_caller_still_waits_for_the_cascade() {
    let (_guard, _, _) = install_deterministic_context(51972);
    let (state, store, barrier) = reaction_fixture(false).await;
    let mut first = Box::pin(run(&state, "Source", false, true));
    tokio::select! { biased;
        reply = &mut first => panic!("returned before child commit: {reply:?}"),
        _ = barrier.entered.notified() => {}
    }
    drop(first);
    let mut retry = Box::pin(run(&state, "Source", false, true));
    pending(&mut retry).await;
    barrier.release.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(1), retry)
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(action_count(&store, "Child", "Start"), 1);
    assert_eq!(action_count(&store, "Leaf", "Start"), 1);
}

#[tokio::test(start_paused = true)]
async fn stronger_reaction_wait_joins_the_already_started_cascade() {
    let (_guard, _, _) = install_deterministic_context(51973);
    let (state, store, barrier) = reaction_fixture(false).await;
    let initial = run(&state, "Source", false, false).await;
    assert!(initial.success);
    barrier.entered.notified().await;
    let mut stronger = Box::pin(run(&state, "Source", false, true));
    pending(&mut stronger).await;
    barrier.release.add_permits(1);
    let final_reply = tokio::time::timeout(Duration::from_secs(1), stronger)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(initial).unwrap(),
        serde_json::to_value(final_reply).unwrap()
    );
    assert_eq!(action_count(&store, "Child", "Start"), 1);
    assert_eq!(action_count(&store, "Leaf", "Start"), 1);
}

#[tokio::test(start_paused = true)]
async fn stronger_integration_wait_preserves_the_first_background_mode() {
    let (_guard, _, _) = install_deterministic_context(51974);
    let (state, _, adapter) = credential_fixture().await;
    let initial = run(&state, "Work", false, false).await;
    assert!(initial.success);
    adapter.started.notified().await;
    let stronger = tokio::time::timeout(Duration::from_secs(1), run(&state, "Work", true, true))
        .await
        .expect("later integration wait attempted an absent milestone upgrade");
    assert_eq!(
        serde_json::to_value(initial).unwrap(),
        serde_json::to_value(stronger).unwrap()
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    adapter.finish.add_permits(1);
    let hash = adapter.hashes.lock().unwrap()[0].clone();
    tokio::time::timeout(Duration::from_secs(1), async {
        while credential_status(&state, &hash).await != "Revoked" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background credential cleanup did not finish");
}

#[tokio::test(start_paused = true)]
async fn reaction_back_edge_reuses_ready_effects_but_independent_waiter_joins_terminal() {
    let (_guard, _, _) = install_deterministic_context(51975);
    let (state, store, barrier) = reaction_fixture(true).await;
    let mut first = Box::pin(run(&state, "Source", false, true));
    tokio::select! { biased;
        result = &mut first => panic!("returned before dependent commit: {result:?}"),
        _ = barrier.entered.notified() => {}
    }
    let mut independent = Box::pin(run(&state, "Source", false, true));
    pending(&mut independent).await;
    barrier.release.add_permits(1);
    let (first, independent) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(first, independent)
    })
    .await
    .expect("reaction back edge waited on its ancestor terminal");
    assert!(first.success);
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(independent).unwrap()
    );
    for kind in ["Source", "Child", "Leaf"] {
        assert_eq!(action_count(&store, kind, "Start"), 1);
    }
}

#[path = "committed_operation_completion/concurrent_cycle.rs"]
mod concurrent_cycle;
