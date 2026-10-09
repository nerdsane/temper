//! A production inline callback must refuse, not await, its own ancestor claim.
use super::*;

async fn start_with_normalized_callback_params(state: &ServerState) -> EntityResponse {
    state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            "TimedTask",
            "task",
            "Start",
            json!({"duration_ms": 0}),
            DispatchExtOptions {
                agent_ctx: &AgentContext {
                    idempotency_key: Some("same-key".into()),
                    ..AgentContext::system()
                },
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn inline_callback_refuses_ancestor_claim_without_stranding_independent_waiters() {
    let (_guard, _, _) = install_deterministic_context(51950);
    let (state, store, adapter) = fixture(51950);
    let spec = SPEC.replace(
        "adapter = \"controlled\"",
        "adapter = \"controlled\"\non_success = \"Start\"",
    );
    state.registry.write().unwrap().register_tenant(
        "default",
        parse_csdl(TASK_CSDL).unwrap(),
        TASK_CSDL.into(),
        &[("TimedTask", &spec)],
    );
    let mut owner = Box::pin(start_with_normalized_callback_params(&state));
    tokio::select! { biased;
        response = &mut owner => panic!("owner returned before adapter: {response:?}"),
        _ = adapter.started.notified() => {}
    }
    let mut retry = Box::pin(start_with_normalized_callback_params(&state));
    assert_waiting(&mut retry, 51950).await;
    // Success({}, 0) is normalized by the production adapter to duration_ms=0,
    // exactly matching the outer action, body and key. The callback reaches
    // effects ownership, not the unrelated different-action mismatch guard.
    adapter.finish.notify_one();
    let (owner, retry) =
        tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(owner, retry) })
            .await
            .expect("inline callback waited on its own effects claim");
    assert!(!owner.success, "ancestor acquisition must be refused");
    assert!(
        owner.error.as_deref().unwrap().contains("ancestor"),
        "{owner:?}"
    );
    assert_eq!(
        serde_json::to_value(&owner).unwrap(),
        serde_json::to_value(&retry).unwrap()
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);

    // The failed attempt must release Pending; another independent request can
    // retry effects and reach the same explicit refusal, rather than hang.
    adapter.finish.notify_one();
    let retried = tokio::time::timeout(
        Duration::from_secs(1),
        start_with_normalized_callback_params(&state),
    )
    .await
    .expect("ancestor refusal stranded the claim");
    assert!(!retried.success);
    assert!(retried.error.as_deref().unwrap().contains("ancestor"));
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        store
            .dump_journal("default:TimedTask:task")
            .iter()
            .filter(|e| e.payload["action"] == "Start")
            .count(),
        1
    );
}
