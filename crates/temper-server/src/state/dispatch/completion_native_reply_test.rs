//! Native final replies, pending adoption mode and cold no-redelivery.
use super::tests::fixture::state_with_specs;
use crate::{
    EntityMsg, EntityResponse, ServerState, StorageStack,
    adapters::{AdapterContext, AdapterError, AdapterRegistry, AdapterResult, AgentAdapter},
    idempotency::ActionReplyMode,
    request_context::AgentContext,
    state::DispatchExtOptions,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

const SPEC: &str = r#"
[automaton]
name="Work"
states=["Idle","Running","Done"]
initial="Idle"
[[action]]
name="Start"
from=["Idle"]
to="Running"
[[action.triggers]]
name="finish"
kind="adapter"
adapter="proof-native"
on_success="Complete"
[[action]]
name="Complete"
from=["Running"]
to="Done"
"#;
struct Native(AtomicUsize);
#[async_trait]
impl AgentAdapter for Native {
    fn adapter_type(&self) -> &str {
        "proof-native"
    }
    async fn execute(&self, _: AdapterContext) -> Result<AdapterResult, AdapterError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(AdapterResult::success(json!({}), 0))
    }
}
fn state(store: &SimEventStore, adapter: Arc<Native>) -> ServerState {
    let mut state = state_with_specs(&[("Work", SPEC)]);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let mut adapters = AdapterRegistry::new();
    adapters.register(adapter);
    state.adapter_registry = Arc::new(adapters);
    state
}
async fn dispatch(state: &ServerState, key: Option<&str>) -> Result<EntityResponse, String> {
    state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            "Work",
            "one",
            "Start",
            json!({}),
            DispatchExtOptions {
                agent_ctx: &AgentContext {
                    idempotency_key: key.map(str::to_owned),
                    ..AgentContext::system()
                },
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
}
#[tokio::test]
async fn native_awaited_reply_is_retained_warm_but_cold_never_reexecutes_adapter() {
    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(52351);
    let store = SimEventStore::no_faults(52323);
    let adapter = Arc::new(Native(AtomicUsize::new(0)));
    let warm = state(&store, adapter.clone());
    let first = dispatch(&warm, None).await.unwrap();
    assert!(first.success, "{:?}", first.error);
    assert_eq!(first.state.status, "Done");
    let events = store.dump_journal("default:Work:one");
    let key = events
        .iter()
        .find(|e| e.payload["action"] == "Start")
        .unwrap()
        .payload["idempotency_key"]
        .as_str()
        .unwrap();
    let child_key = events
        .iter()
        .find(|e| e.payload["action"] == "Complete")
        .unwrap()
        .payload["idempotency_key"]
        .as_str()
        .unwrap();
    assert_ne!(
        key, child_key,
        "callback borrowed actual auto-generated parent key"
    );
    let retained = dispatch(&warm, Some(key)).await.unwrap();
    assert!(retained.success);
    assert_eq!(
        serde_json::to_value(retained.state).unwrap(),
        serde_json::to_value(first.state).unwrap()
    );
    assert_eq!(adapter.0.load(Ordering::SeqCst), 1);
    drop(warm);
    let cold = state(&store, adapter.clone());
    for _ in 0..2 {
        assert!(
            dispatch(&cold, Some(key))
                .await
                .unwrap_err()
                .contains("IdempotencyKeyUnverifiable")
        );
    }
    assert_eq!(adapter.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        serde_json::to_value(store.dump_journal("default:Work:one")).unwrap(),
        serde_json::to_value(events).unwrap()
    );
    assert_eq!(
        cold.get_tenant_entity_state(&TenantId::default(), "Work", "one")
            .await
            .unwrap()
            .state
            .status,
        "Done"
    );
}
#[tokio::test]
async fn pending_actor_commit_retains_first_background_mode_when_stronger_retry_adopts() {
    let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(52352);
    let store = SimEventStore::no_faults(52324);
    let adapter = Arc::new(Native(AtomicUsize::new(0)));
    let state = state(&store, adapter.clone());
    let actor = state
        .get_or_spawn_tenant_actor(&TenantId::default(), "Work", "one")
        .unwrap();
    // Real commit before any dispatcher adopts the Pending entry (reply-loss seam).
    let first: EntityResponse = actor
        .ask(
            EntityMsg::Action {
                name: "Start".into(),
                params: json!({}),
                related: BTreeMap::new(),
                idempotency_key: Some("pending".into()),
                expected_authorization_precondition: None,
                reply_mode: ActionReplyMode::Dispatch {
                    await_integration: false,
                    await_reactions: false,
                    reaction_depth: None,
                },
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();
    assert!(first.success);
    assert_eq!(first.state.status, "Running");
    let retry = dispatch(&state, Some("pending")).await.unwrap();
    assert!(retry.success);
    assert_eq!(
        retry.state.status, "Running",
        "later preferences changed first mode"
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if state
                .get_tenant_entity_state(&TenantId::default(), "Work", "one")
                .await
                .unwrap()
                .state
                .status
                == "Done"
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background callback did not complete");
    let retry = dispatch(&state, Some("pending")).await.unwrap();
    assert!(retry.success);
    assert_eq!(retry.state.status, "Running");
    assert_eq!(adapter.0.load(Ordering::SeqCst), 1);
    let event: Value = store
        .dump_journal("default:Work:one")
        .into_iter()
        .find(|e| e.payload["action"] == "Start")
        .unwrap()
        .payload;
    assert_eq!(
        event["idempotency_reply"]["mode"]["await_integration"],
        false
    );
    assert_eq!(event["idempotency_reply"]["mode"]["await_reactions"], false);
}
