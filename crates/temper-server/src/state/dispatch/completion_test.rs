//! Faults act on the real owned worker and real credential cleanup child.
pub(super) mod fixture {
    use crate as temper_server;
    include!("../../../tests/committed_operation_completion/fixture.rs");
}
use super::*;
use crate::idempotency::OperationReceipt;
use fixture::*;
use futures_util::poll;
use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;
use temper_runtime::scheduler::install_deterministic_context;

async fn pending<F: std::future::Future>(future: &mut std::pin::Pin<Box<F>>) {
    for _ in 0..32 {
        assert!(poll!(future.as_mut()).is_pending());
        tokio::task::yield_now().await;
    }
}

fn receipt(state: &crate::ServerState) -> Arc<OperationReceipt> {
    let request = OperationRequest {
        tenant: temper_runtime::tenant::TenantId::default(),
        entity_type: "Work".into(),
        entity_id: "one".into(),
        action: "Start".into(),
        params: serde_json::json!({}),
        agent_ctx: crate::request_context::AgentContext::system(),
        idempotency_key: Some("K".into()),
        await_integration: true,
        reaction_depth: Some(0),
        detach_reactions: false,
    };
    match state
        .idempotency_cache
        .claim_operation("default:Work:one", "K", request)
    {
        OperationClaim::Join(receipt) => receipt,
        _ => panic!("gated adapter must already have an owned receipt"),
    }
}

#[tokio::test(start_paused = true)]
async fn terminated_worker_pins_surviving_credential_work_even_after_child_cleanup() {
    let (_guard, clock, _) = install_deterministic_context(51977);
    let (state, store, adapter) = credential_fixture().await;
    let mut first = Box::pin(run(&state, "Work", true, true));
    tokio::select! { biased;
        reply = &mut first => panic!("returned before child: {reply:?}"),
        _ = adapter.started.notified() => {}
    }
    let mut retry = Box::pin(run(&state, "Work", true, true));
    pending(&mut retry).await;
    let hash = adapter.hashes.lock().unwrap()[0].clone();
    let receipt = receipt(&state);
    receipt.interrupt_worker();
    let (first, retry) =
        tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(first, retry) })
            .await
            .expect("worker termination stranded waiters");
    assert!(!first.success);
    assert_eq!(first.error, retry.error);
    assert!(first.error.unwrap().contains("completion is unknown"));
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(credential_status(&state, &hash).await, "Active");
    assert!(!run(&state, "Work", true, true).await.success);
    adapter.finish.add_permits(1);
    tokio::time::timeout(Duration::from_secs(1), async {
        while credential_status(&state, &hash).await != "Revoked" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("surviving cleanup did not revoke its credential");
    clock.advance_by((crate::idempotency::IDEMPOTENCY_TTL_SECS as u64 + 1) * 10);
    for n in 0..=crate::idempotency::IDEMPOTENCY_BUDGET_PER_ACTOR {
        state.idempotency_cache.put(
            "default:Work:one",
            &format!("other-{n}"),
            "other-binding",
            receipt.committed.clone(),
        );
    }
    assert!(!run(&state, "Work", true, true).await.success);
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    assert_eq!(action_count(&store, "Work", "Start"), 1);
    assert_eq!(
        action_count_id(&store, "AgentCredential", &hash, "Revoke"),
        1
    );
    assert_eq!(
        state
            .idempotency_cache
            .continuation_slots
            .available_permits(),
        63
    );
    let remaining = state
        .idempotency_cache
        .continuation_slots
        .clone()
        .try_acquire_many_owned(63)
        .unwrap();
    assert!(
        state
            .idempotency_cache
            .continuation_slots
            .clone()
            .try_acquire_owned()
            .is_err()
    );
    drop(remaining);
}

#[tokio::test(start_paused = true)]
async fn descendants_finish_with_all_root_slots_occupied_and_admission_reports_commit() {
    let (_guard, _, _) = install_deterministic_context(51978);
    let (state, store, barrier) = reaction_fixture(false).await;
    let reserved = state
        .idempotency_cache
        .continuation_slots
        .clone()
        .try_acquire_many_owned(63)
        .unwrap();
    let mut first = Box::pin(run(&state, "Source", false, true));
    tokio::select! { biased;
        reply = &mut first => panic!("returned before child: {reply:?}"),
        _ = barrier.entered.notified() => {}
    }
    assert_eq!(
        state
            .idempotency_cache
            .continuation_slots
            .available_permits(),
        0
    );
    let overflow = state
        .dispatch_tenant_action(
            &temper_runtime::tenant::TenantId::default(),
            "Source",
            "overflow",
            "Start",
            serde_json::json!({}),
            &crate::request_context::AgentContext::system(),
        )
        .await
        .unwrap();
    assert!(!overflow.success);
    assert!(
        overflow
            .error
            .unwrap()
            .contains("action committed; completion capacity exhausted")
    );
    assert_eq!(action_count_id(&store, "Source", "overflow", "Start"), 1);
    barrier.release.add_permits(1);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .expect("descendants competed for root capacity")
            .success
    );
    for kind in ["Source", "Child", "Leaf"] {
        assert_eq!(action_count(&store, kind, "Start"), 1);
    }
    assert_eq!(
        state
            .idempotency_cache
            .continuation_slots
            .available_permits(),
        1
    );
    drop(reserved);
    assert_eq!(
        state
            .idempotency_cache
            .continuation_slots
            .available_permits(),
        64
    );
}

#[tokio::test(start_paused = true)]
async fn deliberate_core_only_write_has_no_reaction_obligation() {
    let (_guard, _, _) = install_deterministic_context(51979);
    let (state, store, _) = reaction_fixture(false).await;
    let response = state
        .dispatch_tenant_action_core(
            super::super::DispatchCommand {
                tenant: &temper_runtime::tenant::TenantId::default(),
                entity_type: "Source",
                entity_id: "one",
                action: "Start",
                params: serde_json::json!({}),
                agent_ctx: &crate::request_context::AgentContext::system(),
                await_integration: false,
                await_reactions: true,
            },
            None,
        )
        .await
        .unwrap();
    assert!(response.success);
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(action_count(&store, "Source", "Start"), 1);
    assert_eq!(action_count(&store, "Child", "Start"), 0);
}

#[tokio::test(start_paused = true)]
async fn missing_receipt_after_commit_does_not_launch_uncached_integration() {
    let (_guard, _, _) = install_deterministic_context(51983);
    let (state, store, adapter) = credential_fixture().await;
    *state.idempotency_cache.missing_receipt_once.lock().unwrap() =
        Some(("default:Work:one".into(), "K".into()));
    let response = tokio::time::timeout(Duration::from_secs(1), run(&state, "Work", true, true))
        .await
        .expect("missing receipt launched unowned adapter work");
    assert!(!response.success);
    assert!(
        response
            .error
            .unwrap()
            .contains("action committed; completion receipt unavailable")
    );
    assert_eq!(action_count(&store, "Work", "Start"), 1);
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    assert!(adapter.hashes.lock().unwrap().is_empty());
    // Losing the receipt loses final reply authority, not just permission to
    // restart effects. The committed core digest cannot manufacture success.
    missing_receipt_retry_is_unverifiable(&state, &store, "Work").await;
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    assert_eq!(action_count(&store, "Work", "Start"), 1);
}

#[tokio::test(start_paused = true)]
async fn missing_receipt_after_commit_does_not_launch_uncached_reactions() {
    let (_guard, _, _) = install_deterministic_context(51984);
    let (state, store, barrier) = reaction_fixture(false).await;
    *state.idempotency_cache.missing_receipt_once.lock().unwrap() =
        Some(("default:Source:one".into(), "K".into()));
    let response = tokio::time::timeout(Duration::from_secs(1), run(&state, "Source", false, true))
        .await
        .expect("missing receipt launched unowned reactions");
    assert!(!response.success);
    assert!(
        response
            .error
            .unwrap()
            .contains("action committed; completion receipt unavailable")
    );
    missing_receipt_retry_is_unverifiable(&state, &store, "Source").await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    let mut entered = Box::pin(barrier.entered.notified());
    assert!(
        poll!(entered.as_mut()).is_pending(),
        "a dependent commit was attempted"
    );
    assert_eq!(action_count(&store, "Source", "Start"), 1);
    assert_eq!(action_count(&store, "Child", "Start"), 0);
    assert_eq!(action_count(&store, "Leaf", "Start"), 0);
}

#[tokio::test(start_paused = true)]
async fn ordinary_failed_reaction_is_terminal_not_a_redelivery_request() {
    let (_guard, _, _) = install_deterministic_context(51985);
    let (state, store, barrier) = reaction_fixture(false).await;
    barrier.release.add_permits(1);
    let tenant = temper_runtime::tenant::TenantId::default();
    // Spend Child.Start under another key; the Source rule must now fail its
    // state guard normally, not replay a successful matching child receipt.
    assert!(
        state
            .dispatch_tenant_action(
                &tenant,
                "Child",
                "one",
                "Start",
                serde_json::json!({}),
                &crate::request_context::AgentContext::system()
            )
            .await
            .unwrap()
            .success
    );
    let context = crate::request_context::AgentContext {
        idempotency_key: Some("source-key".into()),
        ..crate::request_context::AgentContext::system()
    };
    let dispatch = || {
        state.dispatch_tenant_action_with_completion(
            super::super::DispatchCommand {
                tenant: &tenant,
                entity_type: "Source",
                entity_id: "one",
                action: "Start",
                params: serde_json::json!({}),
                agent_ctx: &context,
                await_integration: false,
                await_reactions: true,
            },
            None,
            Some(0),
        )
    };
    let first = dispatch().await.unwrap();
    assert!(first.response.success);
    assert_eq!(first.reactions.len(), 1);
    assert!(!first.reactions[0].success);
    let replay = dispatch().await.unwrap();
    assert!(replay.response.success);
    assert!(
        replay.reactions.is_empty(),
        "normal rule failure was redelivered"
    );
    assert_eq!(action_count(&store, "Source", "Start"), 1);
    assert_eq!(action_count(&store, "Child", "Start"), 1);
}

async fn missing_receipt_retry_is_unverifiable(
    state: &crate::ServerState,
    store: &temper_store_sim::SimEventStore,
    kind: &str,
) {
    let tenant = temper_runtime::tenant::TenantId::default();
    let before = state
        .get_tenant_entity_state(&tenant, kind, "one")
        .await
        .unwrap()
        .state;
    assert_eq!(before.status, "Running", "the original action committed");
    let persistence_id = format!("default:{kind}:one");
    let journal = serde_json::to_value(store.dump_journal(&persistence_id)).unwrap();
    let event_count = store.total_events();
    let error = state
        .dispatch_tenant_action_ext_typed(
            &temper_runtime::tenant::TenantId::default(),
            kind,
            "one",
            "Start",
            serde_json::json!({}),
            crate::state::DispatchExtOptions {
                agent_ctx: &crate::request_context::AgentContext {
                    idempotency_key: Some("K".into()),
                    ..crate::request_context::AgentContext::system()
                },
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect_err("lost receipt must not invent final success");
    assert!(
        matches!(
            &error,
            super::super::DispatchError::IdempotencyKeyUnverifiable(_)
        ),
        "{error:?}"
    );
    let after = state
        .get_tenant_entity_state(&tenant, kind, "one")
        .await
        .unwrap()
        .state;
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(before).unwrap(),
        "refusal must not regress current actor state"
    );
    assert_eq!(
        serde_json::to_value(store.dump_journal(&persistence_id)).unwrap(),
        journal,
        "refusal must not change the committed journal"
    );
    assert_eq!(store.total_events(), event_count, "no new writes elsewhere");
}
