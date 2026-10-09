//! Typed native uncertainty must win over both success and failure callbacks.
use super::local_wasm_tests::CSDL;
use super::tests::fixture::*;
use crate::adapters::{AdapterContext, AdapterError, AdapterRegistry, AdapterResult, AgentAdapter};
use crate::request_context::AgentContext;
use crate::state::DispatchExtOptions;
use crate::storage::{BackendLabel, BoxedEventStore};
use async_trait::async_trait;
use serde_json::json;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use temper_runtime::{
    persistence::{
        EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
        PersistenceError,
    },
    tenant::TenantId,
};
use temper_store_sim::SimEventStore;

struct CleanupFaultStore {
    inner: SimEventStore,
    reject: Arc<AtomicBool>,
}
impl EventStore for CleanupFaultStore {
    async fn append(
        &self,
        id: &str,
        seq: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        if self.reject.load(Ordering::SeqCst)
            && id.contains(":AgentCredential:")
            && events.iter().any(|e| e.payload["action"] == "Revoke")
        {
            return Err(PersistenceError::Storage(
                "injected required-cleanup failure".into(),
            ));
        }
        self.inner.append(id, seq, events).await
    }
    async fn append_batch(
        &self,
        a: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        self.inner.append_batch(a).await
    }
    async fn read_events(
        &self,
        id: &str,
        seq: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.inner.read_events(id, seq).await
    }
    async fn save_snapshot(&self, id: &str, seq: u64, data: &[u8]) -> Result<(), PersistenceError> {
        self.inner.save_snapshot(id, seq, data).await
    }
    async fn load_snapshot(&self, id: &str) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        self.inner.load_snapshot(id).await
    }
    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.inner.list_entity_ids(tenant).await
    }
    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        kind: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.inner.list_entity_ids_by_type(tenant, kind).await
    }
}

#[derive(Clone, Copy)]
enum Outcome {
    Success,
    Panic,
    JoinedFailure,
}
struct NativeResult {
    outcome: Outcome,
    hashes: Mutex<Vec<String>>,
}
#[async_trait]
impl AgentAdapter for NativeResult {
    fn adapter_type(&self) -> &str {
        "credential-gate"
    }
    fn requires_platform_credential(&self) -> bool {
        true
    }
    async fn execute(&self, ctx: AdapterContext) -> Result<AdapterResult, AdapterError> {
        self.hashes
            .lock()
            .unwrap()
            .push(crate::identity::hash_token(
                ctx.agent_ctx.agent_api_key.as_deref().unwrap(),
            ));
        match self.outcome {
            Outcome::Success => Ok(AdapterResult::success(json!({}), 0)),
            Outcome::Panic => panic!("injected native worker failure"),
            Outcome::JoinedFailure => {
                Err(AdapterError::Execution("ordinary joined failure".into()))
            }
        }
    }
}

async fn native_result(outcome: Outcome) {
    let (mut state, store, _) = credential_fixture().await;
    let work = state
        .registry
        .read()
        .unwrap()
        .get_spec(&TenantId::default(), "Work")
        .unwrap()
        .ioa_source
        .clone()
        .replace(
            "states=[\"Idle\",\"Running\"]",
            "states=[\"Idle\",\"Running\",\"Done\",\"Failed\"]",
        )
        .replace(
            "adapter=\"credential-gate\"",
            "adapter=\"credential-gate\"\non_success=\"Complete\"\non_failure=\"Fail\"",
        )
        + "\n[[action]]\nname=\"Complete\"\nfrom=[\"Running\"]\nto=\"Done\"\n[[action]]\nname=\"Fail\"\nfrom=[\"Running\"]\nto=\"Failed\"\n";
    state
        .registry
        .write()
        .unwrap()
        .try_register_tenant_with_constraints(
            "default",
            temper_spec::csdl::parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Work", &work)],
            None,
            true,
        )
        .unwrap();
    let fault = Arc::new(AtomicBool::new(matches!(outcome, Outcome::Success)));
    state.storage_stack = Some(Arc::new(crate::StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(CleanupFaultStore {
            inner: store.clone(),
            reject: fault.clone(),
        }),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )));
    let adapter = Arc::new(NativeResult {
        outcome,
        hashes: Mutex::new(Vec::new()),
    });
    let mut adapters = AdapterRegistry::new();
    adapters.register(adapter.clone());
    state.adapter_registry = Arc::new(adapters);
    let invoke = |key| async {
        state
            .dispatch_tenant_action_ext(
                &TenantId::default(),
                "Work",
                "one",
                "Start",
                json!({}),
                DispatchExtOptions {
                    agent_ctx: &AgentContext {
                        idempotency_key: key,
                        ..AgentContext::system()
                    },
                    await_integration: true,
                    await_reactions: true,
                },
            )
            .await
            .unwrap()
    };
    let result = invoke(None).await;
    let key = store
        .dump_journal("default:Work:one")
        .into_iter()
        .find(|e| e.payload["action"] == "Start")
        .unwrap()
        .payload["idempotency_key"]
        .as_str()
        .unwrap()
        .to_owned();
    if matches!(outcome, Outcome::JoinedFailure) {
        assert!(
            result.success,
            "declared failure recovery did not run: {result:?}"
        );
        assert_eq!(action_count(&store, "Work", "Fail"), 1);
    } else {
        assert!(!result.success);
        assert!(result.error.unwrap().contains("completion is unknown"));
        assert_eq!(
            action_count(&store, "Work", "Complete"),
            0,
            "unknown cleanup certified native success"
        );
        assert_eq!(
            action_count(&store, "Work", "Fail"),
            0,
            "unknown native failure launched recovery"
        );
        assert!(!invoke(Some(key)).await.success);
        assert_eq!(adapter.hashes.lock().unwrap().len(), 1);
        assert_eq!(
            state
                .idempotency_cache
                .continuation_slots
                .available_permits(),
            63
        );
    }
    let hash = adapter.hashes.lock().unwrap()[0].clone();
    if matches!(outcome, Outcome::Success) {
        assert_eq!(credential_status(&state, &hash).await, "Active");
        fault.store(false, Ordering::SeqCst);
        assert!(
            state
                .dispatch_tenant_action(
                    &TenantId::default(),
                    "AgentCredential",
                    &hash,
                    "Revoke",
                    json!({}),
                    &AgentContext::system()
                )
                .await
                .unwrap()
                .success
        );
    }
    assert_eq!(credential_status(&state, &hash).await, "Revoked");
}

#[tokio::test]
async fn successful_native_execution_with_unknown_cleanup_cannot_dispatch_success() {
    native_result(Outcome::Success).await;
}
#[tokio::test]
async fn native_worker_uncertainty_cannot_dispatch_failure_recovery() {
    native_result(Outcome::Panic).await;
}
#[tokio::test]
async fn joined_native_failure_still_runs_declared_recovery() {
    native_result(Outcome::JoinedFailure).await;
}

struct CallbackChain {
    extra_calls: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl AgentAdapter for CallbackChain {
    fn adapter_type(&self) -> &str {
        "callback-chain"
    }
    async fn execute(&self, ctx: AdapterContext) -> Result<AdapterResult, AdapterError> {
        if ctx
            .integration_config
            .get("step")
            .is_some_and(|step| step == "extra")
        {
            self.extra_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(AdapterResult::success(json!({}), 0));
        }
        if ctx.trigger_action == "Complete" {
            return Err(AdapterError::CompletionUnknown(
                "injected unjoined callback work".into(),
            ));
        }
        Ok(AdapterResult::success(json!({}), 0))
    }
}

#[tokio::test]
async fn callback_that_establishes_unknown_stops_remaining_result_work() {
    let spec = r#"
[automaton]
name="Chain"
states=["Idle","Running","Completed","Failed"]
initial="Idle"
[[action]]
name="Start"
from=["Idle"]
to="Running"
[[action.triggers]]
name="first"
kind="adapter"
adapter="callback-chain"
on_success="Complete"
[[action.triggers]]
name="extra"
kind="adapter"
adapter="callback-chain"
[action.triggers.config]
step="extra"
[[action]]
name="Complete"
from=["Running"]
to="Completed"
[[action.triggers]]
name="uncertain"
kind="adapter"
adapter="callback-chain"
on_failure="Fail"
[[action]]
name="Fail"
from=["Running","Completed"]
to="Failed"
"#;
    let mut state = state_with_specs(&[("Chain", spec)]);
    let store = SimEventStore::no_faults(51999);
    state.set_storage_stack(crate::StorageStack::from_sim(store.clone(), None));
    let adapter = Arc::new(CallbackChain {
        extra_calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut adapters = AdapterRegistry::new();
    adapters.register(adapter.clone());
    state.adapter_registry = Arc::new(adapters);
    let result = state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            "Chain",
            "one",
            "Start",
            json!({}),
            DispatchExtOptions {
                agent_ctx: &AgentContext::system(),
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .unwrap();
    assert!(!result.success);
    assert!(result.error.unwrap().contains("completion is unknown"));
    assert_eq!(
        action_count(&store, "Chain", "Complete"),
        1,
        "a callback committed before uncertainty was known must not be undone"
    );
    assert_eq!(action_count(&store, "Chain", "Fail"), 0);
    assert_eq!(
        adapter.extra_calls.load(Ordering::SeqCst),
        0,
        "continued integration work after callback established uncertainty"
    );
}
