use async_trait::async_trait;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use temper_runtime::persistence::{
    EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::adapters::{
    AdapterContext, AdapterError, AdapterRegistry, AdapterResult, AgentAdapter,
};
use temper_server::entity_actor::EntityResponse;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tokio::sync::{Notify, Semaphore};

const AGENT_CSDL: &str = include_str!("../../../temper-platform/src/specs/agent_model.csdl.xml");
const CREDENTIAL_SPEC: &str =
    include_str!("../../../temper-platform/src/specs/agent_credential.ioa.toml");
const AGENT_TYPE_SPEC: &str =
    include_str!("../../../temper-platform/src/specs/agent_type.ioa.toml");
const WORK: &str = r#"
[automaton]
name="Work"
states=["Idle","Running"]
initial="Idle"
[[action]]
name="Start"
from=["Idle"]
to="Running"
[[action.triggers]]
name="execute"
kind="adapter"
adapter="credential-gate"
"#;

pub struct CredentialGate {
    pub started: Notify,
    pub finish: Semaphore,
    pub calls: AtomicUsize,
    pub hashes: Mutex<Vec<String>>,
}
#[async_trait]
impl AgentAdapter for CredentialGate {
    fn adapter_type(&self) -> &str {
        "credential-gate"
    }
    fn requires_platform_credential(&self) -> bool {
        true
    }
    async fn execute(&self, ctx: AdapterContext) -> Result<AdapterResult, AdapterError> {
        let token = ctx
            .agent_ctx
            .agent_api_key
            .expect("real platform credential must be minted");
        self.hashes
            .lock()
            .unwrap()
            .push(temper_server::identity::hash_token(&token));
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.finish.acquire().await.unwrap().forget();
        Ok(AdapterResult::success(json!({}), 0))
    }
}

pub fn state_with_specs(specs: &[(&str, &str)]) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(AGENT_CSDL).unwrap(),
        AGENT_CSDL.into(),
        specs,
    );
    let mut state = ServerState::from_registry(ActorSystem::new("completion"), registry);
    state.action_dispatch_timeout = Duration::from_secs(10);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    state.rebuild_reaction_dispatcher();
    state
}

pub async fn credential_fixture() -> (ServerState, SimEventStore, Arc<CredentialGate>) {
    let mut state = state_with_specs(&[
        ("Work", WORK),
        ("AgentType", AGENT_TYPE_SPEC),
        ("AgentCredential", CREDENTIAL_SPEC),
    ]);
    let store = SimEventStore::no_faults(51970);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let adapter = Arc::new(CredentialGate {
        started: Notify::new(),
        finish: Semaphore::new(0),
        calls: AtomicUsize::new(0),
        hashes: Mutex::new(Vec::new()),
    });
    let mut adapters = AdapterRegistry::new();
    adapters.register(adapter.clone());
    state.adapter_registry = Arc::new(adapters);
    let defined = state.dispatch_tenant_action(&TenantId::default(), "AgentType", "kind", "Define", json!({
        "name":"worker", "system_prompt":"test", "tool_set":"local", "model":"test", "max_turns":"1", "adapter_config":"{}", "default_budget_cents":"0"
    }), &AgentContext::system()).await.unwrap();
    assert!(defined.success, "{defined:?}");
    state
        .get_or_create_tenant_entity(
            &TenantId::default(),
            "Work",
            "one",
            json!({"agent_type_id":"kind"}),
        )
        .await
        .unwrap();
    (state, store, adapter)
}

pub async fn credential_status(state: &ServerState, hash: &str) -> String {
    state
        .get_tenant_entity_state(&TenantId::default(), "AgentCredential", hash)
        .await
        .unwrap()
        .state
        .status
}

pub async fn run(
    state: &ServerState,
    kind: &str,
    integration: bool,
    reactions: bool,
) -> EntityResponse {
    state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            kind,
            "one",
            "Start",
            json!({}),
            DispatchExtOptions {
                agent_ctx: &AgentContext {
                    idempotency_key: Some("K".into()),
                    ..AgentContext::system()
                },
                await_integration: integration,
                await_reactions: reactions,
            },
        )
        .await
        .unwrap()
}

pub fn action_count(store: &SimEventStore, kind: &str, action: &str) -> usize {
    action_count_id(store, kind, "one", action)
}
pub fn action_count_id(store: &SimEventStore, kind: &str, id: &str, action: &str) -> usize {
    store
        .dump_journal(&format!("default:{kind}:{id}"))
        .iter()
        .filter(|e| e.payload["action"] == action)
        .count()
}

pub struct CommitBarrier {
    pub entered: Notify,
    pub release: Semaphore,
    armed: AtomicBool,
}
struct GatedStore {
    inner: SimEventStore,
    barriers: BTreeMap<String, Arc<CommitBarrier>>,
}
impl EventStore for GatedStore {
    async fn append(
        &self,
        id: &str,
        seq: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        if let Some(barrier) = self.barriers.get(id)
            && events.iter().any(|e| e.payload["action"] == "Start")
            && barrier.armed.swap(false, Ordering::SeqCst)
        {
            barrier.entered.notify_one();
            barrier.release.acquire().await.unwrap().forget();
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

pub fn reaction_spec(kind: &str, targets: &[&str]) -> String {
    let mut spec = format!(
        r#"
[automaton]
name="{kind}"
states=["Idle","Running"]
initial="Idle"
[[action]]
name="Start"
from=["Idle"]
to="Running"
"#
    );
    for target in targets {
        spec += &format!(
            r#"
[[action.triggers]]
name="to_{target}"
kind="entity"
target_entity="{target}"
target_action="Start"
resolve_target={{kind="same_id"}}
"#
        );
    }
    spec
}
pub async fn reaction_fixture(cycle: bool) -> (ServerState, SimEventStore, Arc<CommitBarrier>) {
    let source = reaction_spec("Source", &["Child"]);
    let child = reaction_spec(
        "Child",
        if cycle {
            &["Source", "Leaf"]
        } else {
            &["Leaf"]
        },
    );
    let leaf = reaction_spec("Leaf", &[]);
    let mut state = state_with_specs(&[("Source", &source), ("Child", &child), ("Leaf", &leaf)]);
    let store = SimEventStore::no_faults(51971);
    let barriers = gate_commits(&mut state, store.clone(), &["Child"]);
    (state, store, barriers["Child"].clone())
}

pub fn gate_commits(
    state: &mut ServerState,
    store: SimEventStore,
    kinds: &[&str],
) -> BTreeMap<String, Arc<CommitBarrier>> {
    let barriers: BTreeMap<_, _> = kinds
        .iter()
        .map(|kind| {
            (
                kind.to_string(),
                Arc::new(CommitBarrier {
                    entered: Notify::new(),
                    release: Semaphore::new(0),
                    armed: AtomicBool::new(true),
                }),
            )
        })
        .collect();
    state.storage_stack = Some(Arc::new(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(GatedStore {
            inner: store,
            barriers: barriers
                .iter()
                .map(|(kind, barrier)| (format!("default:{kind}:one"), barrier.clone()))
                .collect(),
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
    barriers
}
