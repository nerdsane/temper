use super::*;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{DeterministicIdGen, LogicalClock, install_sim_context};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tokio::sync::{Notify, oneshot};

use crate::storage::StorageStack;

const AGENT_CSDL: &str = include_str!("../../../../temper-platform/src/specs/agent_model.csdl.xml");
const AGENT_TYPE_IOA: &str =
    include_str!("../../../../temper-platform/src/specs/agent_type.ioa.toml");
const AGENT_CREDENTIAL_IOA: &str =
    include_str!("../../../../temper-platform/src/specs/agent_credential.ioa.toml");
const PROVIDER_SECRET: &str = "provider-super-secret";

struct PersistedIdentityFixture {
    first: crate::state::ServerState,
    second: crate::state::ServerState,
    store: TursoEventStore,
    _directory: tempfile::TempDir,
}

async fn persisted_identity_fixture() -> PersistedIdentityFixture {
    let csdl = parse_csdl(AGENT_CSDL).expect("agent CSDL should parse");
    let mut registry = crate::registry::SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        AGENT_CSDL.to_string(),
        &[
            ("AgentType", AGENT_TYPE_IOA),
            ("AgentCredential", AGENT_CREDENTIAL_IOA),
        ],
    );
    let second_registry = registry.clone();
    let mut first = crate::state::ServerState::from_registry(
        ActorSystem::new("adapter-credential-first"),
        registry,
    );
    let mut second = crate::state::ServerState::from_registry(
        ActorSystem::new("adapter-credential-second"),
        second_registry,
    );
    let directory = tempfile::tempdir().expect("create adapter credential test directory");
    let database_url = format!("file:{}", directory.path().join("identity.db").display());
    let store = TursoEventStore::new(&database_url, None)
        .await
        .expect("create adapter credential store");
    first.set_storage_stack(StorageStack::from_turso(store.clone()));
    second.set_storage_stack(StorageStack::from_turso(store.clone()));

    let response = first
        .dispatch_tenant_action(
            &TenantId::default(),
            "AgentType",
            "adapter-agent-type",
            "Define",
            serde_json::json!({
                "name": "adapter-agent",
                "system_prompt": "test",
                "tool_set": "local",
                "model": "test",
                "max_turns": "1",
                "adapter_config": "{}",
                "default_budget_cents": "0"
            }),
            &AgentContext::system(),
        )
        .await
        .expect("define adapter AgentType");
    assert!(response.success, "Define failed: {:?}", response.error);

    PersistedIdentityFixture {
        first,
        second,
        store,
        _directory: directory,
    }
}

fn credential_entity_state() -> EntityState {
    EntityState {
        entity_type: "AdapterRun".to_string(),
        entity_id: "run-1".to_string(),
        status: "Running".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: serde_json::json!({"agent_type_id": "adapter-agent-type"}),
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: BTreeMap::new(),
    }
}

#[test]
fn adapter_credential_uses_both_uuid_sources_and_has_opaque_shape() {
    let first = uuid::Uuid::from_u128(1);
    let second = uuid::Uuid::from_u128(2);
    let token = derive_adapter_credential_plaintext(first, second);

    assert_eq!(token.len(), "tmpr_".len() + 64);
    assert!(
        token
            .strip_prefix("tmpr_")
            .is_some_and(|material| material.bytes().all(|byte| byte.is_ascii_hexdigit()))
    );
    assert_ne!(
        token,
        derive_adapter_credential_plaintext(uuid::Uuid::from_u128(3), second),
        "the first independent UUID must contribute to the token"
    );
    assert_ne!(
        token,
        derive_adapter_credential_plaintext(first, uuid::Uuid::from_u128(3)),
        "the second independent UUID must contribute to the token"
    );
    assert!(!token.contains(&first.to_string()));
    assert!(!token.contains(&second.to_string()));
}

fn adapter_context(token: String) -> AdapterContext {
    AdapterContext {
        tenant: "default".to_string(),
        entity_type: "AdapterRun".to_string(),
        entity_id: "run-1".to_string(),
        trigger_action: "Run".to_string(),
        trigger_params: serde_json::json!({}),
        entity_state: serde_json::json!({}),
        integration_config: BTreeMap::new(),
        agent_ctx: AdapterAgentContext {
            agent_api_key: Some(token),
            ..AdapterAgentContext::default()
        },
        secrets: BTreeMap::from([("OPENAI_API_KEY".to_string(), PROVIDER_SECRET.to_string())]),
    }
}

async fn mint(fixture: &PersistedIdentityFixture) -> MintedAdapterCredential {
    fixture
        .first
        .mint_agent_credential_if_needed(
            &TenantId::default(),
            &credential_entity_state(),
            &AgentContext::system(),
        )
        .await
        .expect("credential Issue should succeed")
        .expect("agent_type_id should request a credential")
}

async fn assert_token_resolves(state: &crate::state::ServerState, token: &str, expected: bool) {
    let resolved = crate::identity::IdentityResolver::new()
        .resolve(state, &TenantId::default(), token)
        .await;
    assert_eq!(resolved.is_some(), expected);
}

struct ResultAdapter {
    captured: Mutex<Option<oneshot::Sender<String>>>,
    fails: bool,
}

#[async_trait]
impl AgentAdapter for ResultAdapter {
    fn adapter_type(&self) -> &str {
        "credential-result-test"
    }

    async fn execute(&self, ctx: AdapterContext) -> Result<AdapterResult, AdapterError> {
        let token = ctx
            .agent_ctx
            .agent_api_key
            .expect("test adapter should receive credential");
        let provider_secret = ctx
            .secrets
            .get("OPENAI_API_KEY")
            .expect("test adapter should receive provider secret");
        let captured = token.clone();
        if let Some(sender) = self
            .captured
            .lock()
            .expect("capture lock should be healthy")
            .take()
        {
            let _ = sender.send(captured);
        }
        if self.fails {
            Err(AdapterError::Execution(format!(
                "injected adapter error containing {token} and {provider_secret}"
            )))
        } else {
            Ok(AdapterResult::success(
                serde_json::json!({
                    "echo": token,
                    format!("key-{token}"): [
                        format!("nested-{token}"),
                        format!("provider-{provider_secret}")
                    ]
                }),
                1,
            ))
        }
    }
}

struct BlockingAdapter {
    started: Arc<Notify>,
    finish: Arc<Notify>,
}

struct PanicAdapter;

#[async_trait]
impl AgentAdapter for PanicAdapter {
    fn adapter_type(&self) -> &str {
        "credential-panic-test"
    }

    async fn execute(&self, _ctx: AdapterContext) -> Result<AdapterResult, AdapterError> {
        panic!("injected adapter panic")
    }
}

struct NeverCompletesAdapter {
    started: Arc<Notify>,
}

#[async_trait]
impl AgentAdapter for NeverCompletesAdapter {
    fn adapter_type(&self) -> &str {
        "credential-timeout-test"
    }

    async fn execute(&self, _ctx: AdapterContext) -> Result<AdapterResult, AdapterError> {
        self.started.notify_one();
        std::future::pending().await
    }
}

#[async_trait]
impl AgentAdapter for BlockingAdapter {
    fn adapter_type(&self) -> &str {
        "credential-cancellation-test"
    }

    async fn execute(&self, _ctx: AdapterContext) -> Result<AdapterResult, AdapterError> {
        self.started.notify_one();
        self.finish.notified().await;
        Ok(AdapterResult::success(serde_json::json!({}), 1))
    }
}

#[tokio::test]
async fn adapter_success_revokes_captured_token_and_never_persists_plaintext() {
    let fixture = persisted_identity_fixture().await;
    let credential = mint(&fixture).await;
    let plaintext = credential.plaintext.clone();
    let key_hash = credential.key_hash.clone();
    assert_token_resolves(&fixture.second, &plaintext, true).await;
    let (sender, receiver) = oneshot::channel();

    let result = fixture
        .first
        .execute_adapter_with_credential_cleanup(
            Arc::new(ResultAdapter {
                captured: Mutex::new(Some(sender)),
                fails: false,
            }),
            adapter_context(credential.plaintext),
            &TenantId::default(),
            Some(credential.key_hash),
        )
        .await
        .expect("adapter should succeed");
    assert!(result.success);
    let serialized_result = serde_json::to_string(&result).expect("serialize adapter result");
    assert!(!serialized_result.contains(&plaintext));
    assert!(!serialized_result.contains(PROVIDER_SECRET));
    assert!(serialized_result.contains(REDACTED_ADAPTER_CREDENTIAL));
    assert!(serialized_result.contains(REDACTED_ADAPTER_SECRET));
    assert_eq!(
        receiver.await.expect("adapter should capture token"),
        plaintext
    );
    assert_token_resolves(&fixture.second, &plaintext, false).await;

    let events = fixture
        .store
        .read_events(&format!("default:AgentCredential:{key_hash}"), 0)
        .await
        .expect("read credential journal");
    let durable_json = serde_json::to_string(&events).expect("serialize credential events");
    assert!(!durable_json.contains(&plaintext));
}

#[tokio::test]
async fn adapter_error_still_revokes_captured_token() {
    let fixture = persisted_identity_fixture().await;
    let credential = mint(&fixture).await;
    let plaintext = credential.plaintext.clone();
    let (sender, receiver) = oneshot::channel();

    let error = fixture
        .first
        .execute_adapter_with_credential_cleanup(
            Arc::new(ResultAdapter {
                captured: Mutex::new(Some(sender)),
                fails: true,
            }),
            adapter_context(credential.plaintext),
            &TenantId::default(),
            Some(credential.key_hash),
        )
        .await
        .expect_err("adapter error should propagate after cleanup");
    assert!(error.to_string().contains("injected adapter error"));
    assert!(!error.to_string().contains(&plaintext));
    assert!(!error.to_string().contains(PROVIDER_SECRET));
    assert!(error.to_string().contains(REDACTED_ADAPTER_CREDENTIAL));
    assert!(error.to_string().contains(REDACTED_ADAPTER_SECRET));
    assert_eq!(
        receiver.await.expect("adapter should capture token"),
        plaintext
    );
    assert_token_resolves(&fixture.second, &plaintext, false).await;
}

#[tokio::test]
async fn adapter_panic_is_contained_and_still_revokes_token() {
    let fixture = persisted_identity_fixture().await;
    let credential = mint(&fixture).await;
    let plaintext = credential.plaintext.clone();

    let error = fixture
        .first
        .execute_adapter_with_credential_cleanup(
            Arc::new(PanicAdapter),
            adapter_context(credential.plaintext),
            &TenantId::default(),
            Some(credential.key_hash),
        )
        .await
        .expect_err("adapter panic should become a typed error after cleanup");
    assert!(error.to_string().contains("adapter invocation panicked"));
    assert_token_resolves(&fixture.second, &plaintext, false).await;
}

#[tokio::test(start_paused = true)]
async fn adapter_timeout_revokes_token_at_the_execution_budget() {
    let fixture = persisted_identity_fixture().await;
    let credential = mint(&fixture).await;
    let plaintext = credential.plaintext.clone();
    let state = fixture.first.clone();
    let started = Arc::new(Notify::new());
    let started_for_adapter = started.clone();
    let execution = tokio::spawn(async move {
        state
            .execute_adapter_with_credential_cleanup(
                Arc::new(NeverCompletesAdapter {
                    started: started_for_adapter,
                }),
                adapter_context(credential.plaintext),
                &TenantId::default(),
                Some(credential.key_hash),
            )
            .await
    });

    started.notified().await;
    tokio::time::advance(Duration::from_secs(ADAPTER_INVOCATION_BUDGET_SECS + 1)).await;
    let error = execution
        .await
        .expect("execution task should stay healthy")
        .expect_err("adapter should exceed its execution budget");
    assert!(
        error
            .to_string()
            .contains("exceeded its 3600-second budget")
    );
    assert_token_resolves(&fixture.second, &plaintext, false).await;
}

#[tokio::test]
async fn caller_cancellation_detaches_cleanup_and_revokes_after_adapter_finishes() {
    let fixture = persisted_identity_fixture().await;
    let credential = mint(&fixture).await;
    let plaintext = credential.plaintext.clone();
    let started = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let state = fixture.first.clone();
    let started_for_adapter = started.clone();
    let finish_for_adapter = finish.clone();
    let caller = tokio::spawn(async move {
        state
            .execute_adapter_with_credential_cleanup(
                Arc::new(BlockingAdapter {
                    started: started_for_adapter,
                    finish: finish_for_adapter,
                }),
                adapter_context(credential.plaintext),
                &TenantId::default(),
                Some(credential.key_hash),
            )
            .await
    });

    started.notified().await;
    caller.abort();
    finish.notify_one();
    assert!(
        caller
            .await
            .expect_err("caller task should be cancelled")
            .is_cancelled()
    );

    for _ in 0..100 {
        if crate::identity::IdentityResolver::new()
            .resolve(&fixture.second, &TenantId::default(), &plaintext)
            .await
            .is_none()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("detached cleanup did not revoke the captured credential");
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_expiry_denies_token_from_second_state_without_cleanup() {
    let clock = Arc::new(LogicalClock::with_delta_ms(1_000));
    let id_gen = Arc::new(DeterministicIdGen::new(42));
    let _clock_guard = install_sim_context(clock.clone(), id_gen);
    let fixture = persisted_identity_fixture().await;
    let credential = mint(&fixture).await;
    let plaintext = credential.plaintext.clone();
    assert_token_resolves(&fixture.second, &plaintext, true).await;

    clock.advance_by(ADAPTER_CREDENTIAL_TTL_SECS as u64 + 1);
    assert_token_resolves(&fixture.second, &plaintext, false).await;

    fixture
        .first
        .revoke_minted_adapter_credential(&TenantId::default(), &credential.key_hash)
        .await
        .expect("expired credential should still revoke durably");
}

#[tokio::test]
async fn adapter_credential_mint_requires_caller_delegation_authority() {
    let fixture = persisted_identity_fixture().await;
    let unprivileged = AgentContext {
        security_ctx: Some(temper_authz::SecurityContext::from_resolved_identity(
            "unprivileged-agent",
            "worker",
            None,
        )),
        ..AgentContext::default()
    };
    let before = fixture
        .first
        .list_entity_ids(&TenantId::default(), "AgentCredential");

    let result = fixture
        .first
        .mint_agent_credential_if_needed(
            &TenantId::default(),
            &credential_entity_state(),
            &unprivileged,
        )
        .await;
    let Err(error) = result else {
        panic!("default-deny caller minted an adapter credential");
    };

    assert!(error.contains("delegation denied"), "{error}");
    assert_eq!(
        fixture
            .first
            .list_entity_ids(&TenantId::default(), "AgentCredential"),
        before
    );
}
