//! Controlled adapter I/O; production dispatch and actors run on a paused,
//! single-thread scheduler over the seeded in-memory journal.
use super::*;
use async_trait::async_trait;
use futures_util::poll;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use temper_server::adapters::{
    AdapterContext, AdapterError, AdapterRegistry, AdapterResult, AgentAdapter,
};
use temper_server::entity_actor::EntityResponse;
use temper_server::state::DispatchExtOptions;
use tokio::sync::Notify;

const TASK_CSDL: &str = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="TimedTask"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Tasks" EntityType="Test.TimedTask"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

const SPEC: &str = r#"
[automaton]
name = "TimedTask"
states = ["Idle", "Running", "Done"]
initial = "Idle"
[[action]]
name = "Start"
from = ["Idle"]
to = "Running"
[[action.triggers]]
name = "work"
kind = "adapter"
adapter = "controlled"
[[action]]
name = "TimeoutFail"
from = ["Running"]
to = "Done"
[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "TimeoutFail"
"#;

#[derive(Default)]
struct ControlledAdapter {
    started: Notify,
    finish: Notify,
    fail: AtomicBool,
    calls: AtomicUsize,
}

#[async_trait]
impl AgentAdapter for ControlledAdapter {
    fn adapter_type(&self) -> &str {
        "controlled"
    }
    async fn execute(&self, _: AdapterContext) -> Result<AdapterResult, AdapterError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.finish.notified().await;
        if self.fail.load(Ordering::SeqCst) {
            Err(AdapterError::Execution("injected owner failure".into()))
        } else {
            Ok(AdapterResult::success(json!({}), 0))
        }
    }
}

fn fixture(seed: u64) -> (ServerState, SimEventStore, Arc<ControlledAdapter>) {
    let (mut state, store) = build_state_with_sim_store(seed);
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(TASK_CSDL).unwrap(),
        TASK_CSDL.into(),
        &[("TimedTask", SPEC)],
    );
    state.registry = Arc::new(std::sync::RwLock::new(registry));
    let adapter = Arc::new(ControlledAdapter::default());
    let mut adapters = AdapterRegistry::new();
    adapters.register(adapter.clone());
    state.adapter_registry = Arc::new(adapters);
    (state, store, adapter)
}

async fn start(state: &ServerState) -> EntityResponse {
    state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            "TimedTask",
            "task",
            "Start",
            json!({}),
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

// Drive a pending HTTP/actor request to its effects wait without wall-clock
// sleeps. The adapter owner stays pinned by the explicit finish notification.
async fn assert_waiting(
    future: &mut std::pin::Pin<Box<impl std::future::Future<Output = EntityResponse>>>,
    seed: u64,
) {
    for _ in 0..32 {
        assert!(
            poll!(future.as_mut()).is_pending(),
            "seed {seed}: duplicate returned before effects owner finished"
        );
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn concurrent_idempotency_retry_waits_for_owner_final_response() {
    for seed in 51900..51908 {
        let (_guard, _, _) = install_deterministic_context(seed);
        let (state, store, adapter) = fixture(seed);
        let mut owner = Box::pin(start(&state));
        tokio::select! { biased;
            response = &mut owner => panic!("owner returned before adapter: {response:?}"),
            _ = adapter.started.notified() => {}
        }
        let mut retries = Vec::new();
        for _ in 0..=seed % 3 {
            let mut retry = Box::pin(start(&state));
            assert_waiting(&mut retry, seed).await;
            retries.push(retry);
        }
        if seed % 2 == 0 {
            // Cancelling a waiter must not release another dispatch's claim.
            let mut cancelled = Box::pin(start(&state));
            assert_waiting(&mut cancelled, seed).await;
            drop(cancelled);
            retries.reverse();
        }
        adapter.finish.notify_one();
        let (owner, retries) = tokio::join!(owner, futures_util::future::join_all(retries));
        assert!(owner.success, "{owner:?}");
        assert_eq!(owner.state.status, "Running");
        for retry in retries {
            assert_eq!(
                serde_json::to_value(&retry).unwrap(),
                serde_json::to_value(&owner).unwrap()
            );
        }
        let warm = start(&state).await;
        assert_eq!(
            serde_json::to_value(&warm).unwrap(),
            serde_json::to_value(&owner).unwrap()
        );
        assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store
                .dump_journal("default:TimedTask:task")
                .iter()
                .filter(|e| e.payload["action"] == "Start")
                .count(),
            1
        );
    }
}

#[tokio::test(start_paused = true)]
async fn concurrent_idempotency_retry_shares_failure_then_recovers() {
    let (_guard, _, _) = install_deterministic_context(51910);
    let (state, _, adapter) = fixture(51910);
    adapter.fail.store(true, Ordering::SeqCst);
    let mut owner = Box::pin(start(&state));
    tokio::select! { biased;
        response = &mut owner => panic!("owner returned before adapter: {response:?}"),
        _ = adapter.started.notified() => {}
    }
    let mut retry = Box::pin(start(&state));
    assert_waiting(&mut retry, 51910).await;
    adapter.finish.notify_one();
    let (owner, retry) = tokio::join!(owner, retry);
    assert!(!owner.success);
    assert_eq!(retry.error, owner.error);
    assert!(!retry.success);
    adapter.fail.store(false, Ordering::SeqCst);
    adapter.finish.notify_one();
    assert_eq!(start(&state).await.state.status, "Running");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn cancelled_caller_leaves_owned_execution_for_waiter() {
    let (_guard, _, _) = install_deterministic_context(51911);
    let (state, _, adapter) = fixture(51911);
    let mut owner = Box::pin(start(&state));
    tokio::select! { biased;
        response = &mut owner => panic!("owner returned before adapter: {response:?}"),
        _ = adapter.started.notified() => {}
    }
    let mut retry = Box::pin(start(&state));
    assert_waiting(&mut retry, 51911).await;
    drop(owner);
    adapter.finish.notify_one();
    let response = tokio::time::timeout(Duration::from_secs(1), retry)
        .await
        .expect("cancelled owner stranded claim");
    assert!(response.success, "{response:?}");
    assert_eq!(response.state.status, "Running");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
}

#[path = "http.rs"]
mod http;

#[path = "ancestor.rs"]
mod ancestor;

#[path = "detached.rs"]
mod detached;
