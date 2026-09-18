//! Real dispatcher, actor, persistence, and recovery regression scenarios.
//!
//! Current-thread actor execution and seeded environment responses exercise the
//! production code. This complements the scheduler-controlled DST harness; it
//! does not simulate real provider timing or replace that harness.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use temper_jit::table::TransitionTable;
use temper_runtime::actor::ActorRef;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_store_sim::SimEventStore;

use super::provider::SystemOneProvider;
use crate::entity_actor::{EntityActor, EntityMsg, EntityResponse};
use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::storage::{BackendLabel, BoxedEventStore};
use crate::{ServerState, StorageStack};

const TENANT: &str = "tenant-a";
const ENTITY: &str = "case-1";
const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices>
<Schema Namespace="Temper.SystemOneTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Case"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="Status" Type="Edm.String"/>
<Property Name="Messages" Type="Edm.String"/>
<Property Name="revision" Type="Edm.Int64"/></EntityType>
<EntityContainer Name="Container"><EntitySet Name="Cases" EntityType="Temper.SystemOneTest.Case"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;
const SPEC: &str = r#"
[automaton]
name = "Case"
states = ["Open", "Escalated"]
initial = "Open"
allow_indefinite_states = ["Open", "Escalated"]
[[state]]
name = "Messages"
type = "string"
initial = "Please connect me with a human."
[[state]]
name = "revision"
type = "counter"
initial = "0"
[[action]]
name = "Escalate"
kind = "input"
from = ["Open"]
to = "Escalated"
guard = [{ type = "system_one", model = "jev-latest", state = { ref = "entity.Messages" }, questions = { human = { type = "noul", instructions = "Has a human been requested?" } }, assert = "answers.human.noul >= 0.8" }]
[[action]]
name = "Bump"
from = ["Open"]
effect = [{type="increment", var="revision"}]
"#;

#[derive(Clone, Copy)]
enum Answer {
    Positive,
    Negative,
    Failure,
    PerTenant,
}

enum Mutation {
    Bump(ActorRef<EntityMsg>),
    Reload(Arc<RwLock<TransitionTable>>),
    RemoveGuards(Arc<RwLock<TransitionTable>>),
}

struct TestProvider {
    answer: Answer,
    seed: u64,
    calls: AtomicUsize,
    requests: Mutex<Vec<(String, Value)>>,
    mutation: Mutex<Option<Mutation>>,
}

impl TestProvider {
    fn new(seed: u64, answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            answer,
            seed,
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            mutation: Mutex::new(None),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SystemOneProvider for TestProvider {
    async fn evaluate(&self, tenant: &str, request: &Value) -> Result<Value, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests
            .lock()
            .unwrap()
            .push((tenant.into(), request.clone()));
        for _ in 0..self.seed % 4 {
            tokio::task::yield_now().await;
        }
        let mutation = self.mutation.lock().unwrap().take();
        match mutation {
            Some(Mutation::Bump(actor)) => {
                let result: EntityResponse = actor
                    .ask(action("Bump", json!({}), None), Duration::from_secs(5))
                    .await
                    .expect("competing actor must reply");
                assert!(
                    result.success,
                    "seed={}: competing Bump failed: {:?}",
                    self.seed, result.error
                );
            }
            Some(Mutation::Reload(table)) => {
                *table.write().unwrap() = TransitionTable::from_ioa_source(&SPEC.replace(
                    "Has a human been requested?",
                    "Is a human explicitly requested?",
                ));
            }
            Some(Mutation::RemoveGuards(table)) => {
                let unguarded = SPEC
                    .lines()
                    .filter(|line| !line.starts_with("guard = "))
                    .collect::<Vec<_>>()
                    .join("\n");
                *table.write().unwrap() = TransitionTable::from_ioa_source(&unguarded);
            }
            None => {}
        }
        let value = match self.answer {
            Answer::Failure => return Err("simulated provider unavailable".into()),
            Answer::Negative => 0.2,
            Answer::PerTenant if tenant != TENANT => 0.2,
            _ => 0.9,
        };
        Ok(json!({"model":"jev-sim", "answers":{"human":{"type":"noul", "noul":value}}}))
    }
}

#[path = "actor_tests/hot_swap_test.rs"]
mod hot_swap;

#[path = "actor_tests/occ_hot_swap_test.rs"]
mod occ_hot_swap;

fn server(sim: SimEventStore, provider: Arc<TestProvider>) -> ServerState {
    let mut registry = SpecRegistry::new();
    for tenant in [TENANT, "tenant-b"] {
        registry.register_tenant(
            tenant,
            temper_spec::csdl::parse_csdl(CSDL).unwrap(),
            CSDL.into(),
            &[("Case", SPEC)],
        );
    }
    let mut state =
        ServerState::from_registry(ActorSystem::new("system-one-actor-tests"), registry)
            .with_system_one_provider(provider);
    state.set_storage_stack(StorageStack::from_sim(sim, None));
    for tenant in [TENANT, "tenant-b"] {
        state
            .authz
            .reload_tenant_policies(tenant, "permit(principal, action, resource);")
            .unwrap();
        // Exercise real bootstrap persistence with an explicit record value.
        // Legacy declarations do not imply a stored non-lifecycle field.
        state
            .get_or_spawn_tenant_actor_with_fields(
                &TenantId::new(tenant),
                "Case",
                ENTITY,
                json!({"Messages":"Please connect me with a human.","revision":0}),
            )
            .unwrap();
    }
    state
}

fn action(name: &str, params: Value, attempt: Option<&str>) -> EntityMsg {
    EntityMsg::Action {
        name: name.into(),
        params,
        cross_entity_booleans: BTreeMap::new(),
        idempotency_key: attempt.map(str::to_owned),
        expected_authorization_precondition: None,
    }
}

async fn dispatch(
    state: &ServerState,
    tenant: &str,
    name: &str,
    params: Value,
    attempt: &str,
) -> Result<EntityResponse, String> {
    let mut context = AgentContext::for_service("system-one-test");
    context.idempotency_key = Some(attempt.into());
    state
        .dispatch_tenant_action(
            &TenantId::new(tenant),
            "Case",
            ENTITY,
            name,
            params,
            &context,
        )
        .await
}

async fn read(state: &ServerState, tenant: &str) -> EntityResponse {
    state
        .get_tenant_entity_state(&TenantId::new(tenant), "Case", ENTITY)
        .await
        .unwrap()
}

fn rejected(result: &Result<EntityResponse, String>) -> bool {
    result.as_ref().map_or(true, |response| !response.success)
}

#[tokio::test(flavor = "current_thread")]
async fn successful_guard_commits_receipt_refs_and_replays_without_inference() {
    let _context = install_deterministic_context(801);
    let sim = SimEventStore::no_faults(801);
    let provider = TestProvider::new(801, Answer::Positive);
    let state = server(sim.clone(), provider.clone());
    let first = dispatch(&state, TENANT, "Escalate", json!({}), "attempt")
        .await
        .unwrap();
    assert!(first.success, "{:?}", first.error);
    assert_eq!(first.state.status, "Escalated");
    assert_eq!(provider.calls(), 1);
    let events = sim.dump_journal(&format!("{TENANT}:Case:{ENTITY}"));
    assert_eq!(events.len(), 2);
    let receipts = events[1].payload["system_one_receipts"].as_array().unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(sim.dump_journal(receipts[0].as_str().unwrap()).len(), 1);
    let duplicate = dispatch(&state, TENANT, "Escalate", json!({}), "attempt")
        .await
        .unwrap();
    assert!(duplicate.success);
    assert_eq!(provider.calls(), 1);
    assert_eq!(
        sim.dump_journal(&format!("{TENANT}:Case:{ENTITY}")).len(),
        2
    );
    let disconnected = TestProvider::new(802, Answer::Failure);
    let restarted = server(sim.clone(), disconnected.clone());
    let replayed = read(&restarted, TENANT).await;
    assert_eq!(replayed.state.status, "Escalated");
    assert_eq!(replayed.state.sequence_nr, first.state.sequence_nr);
    let duplicate = dispatch(&restarted, TENANT, "Escalate", json!({}), "attempt")
        .await
        .unwrap();
    assert!(duplicate.success);
    assert_eq!(disconnected.calls(), 0);
    assert_eq!(
        sim.dump_journal(&format!("{TENANT}:Case:{ENTITY}")).len(),
        2
    );
}

#[tokio::test(flavor = "current_thread")]
async fn negative_and_failed_answers_are_durable_without_domain_events() {
    for (seed, answer) in [(803, Answer::Negative), (804, Answer::Failure)] {
        let _context = install_deterministic_context(seed);
        let sim = SimEventStore::no_faults(seed);
        let provider = TestProvider::new(seed, answer);
        let state = server(sim.clone(), provider.clone());
        for _ in 0..3 {
            let result = dispatch(&state, TENANT, "Escalate", json!({}), "same-attempt").await;
            assert!(
                rejected(&result),
                "seed={seed}: negative evaluation committed"
            );
        }
        assert_eq!(provider.calls(), 1, "seed={seed}: retry resampled");
        assert_eq!(read(&state, TENANT).await.state.status, "Open");
        assert_eq!(
            sim.dump_journal(&format!("{TENANT}:Case:{ENTITY}")).len(),
            1
        );
        let restarted_provider = TestProvider::new(seed, Answer::Positive);
        let restarted = server(sim.clone(), restarted_provider.clone());
        let result = dispatch(&restarted, TENANT, "Escalate", json!({}), "same-attempt").await;
        assert!(
            rejected(&result),
            "seed={seed}: recovery resampled a negative receipt"
        );
        assert_eq!(restarted_provider.calls(), 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn denied_outbound_authority_spends_no_model_calls() {
    let _context = install_deterministic_context(805);
    let provider = TestProvider::new(805, Answer::Positive);
    let state = server(SimEventStore::no_faults(805), provider.clone());
    state.authz.reload_tenant_policies(TENANT, "").unwrap();
    let result = dispatch(&state, TENANT, "Escalate", json!({}), "attempt").await;
    assert!(rejected(&result));
    assert_eq!(provider.calls(), 0);
    assert_eq!(read(&state, TENANT).await.state.status, "Open");
}

#[tokio::test(flavor = "current_thread")]
async fn actor_refuses_missing_evidence_even_with_fabricated_guard_booleans() {
    let _context = install_deterministic_context(806);
    let provider = TestProvider::new(806, Answer::Positive);
    let state = server(SimEventStore::no_faults(806), provider.clone());
    let actor = state
        .get_or_spawn_tenant_actor(&TenantId::new(TENANT), "Case", ENTITY)
        .unwrap();
    let _: EntityResponse = actor
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .unwrap();
    let table = TransitionTable::from_ioa_source(SPEC);
    let key = super::collect_guards(&table, "Escalate")[0].key();
    let command = EntityMsg::Action {
        name: "Escalate".into(),
        params: json!({"answers":{"human":{"noul":1}}}),
        cross_entity_booleans: BTreeMap::from([(key, true)]),
        idempotency_key: Some("fabricated-attempt".into()),
        expected_authorization_precondition: None,
    };
    let response: EntityResponse = actor.ask(command, Duration::from_secs(5)).await.unwrap();
    assert!(!response.success);
    assert_eq!(response.state.status, "Open");
    assert!(response.error.unwrap().contains("evidence"));
    assert_eq!(provider.calls(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn stale_inputs_and_spec_never_commit_across_many_actor_schedules() {
    for seed in 0..32 {
        let _context = install_deterministic_context(900 + seed);
        let sim = SimEventStore::no_faults(900 + seed);
        let provider = TestProvider::new(seed, Answer::Positive);
        let state = server(sim.clone(), provider.clone());
        let tenant = TenantId::new(TENANT);
        let actor = state
            .get_or_spawn_tenant_actor(&tenant, "Case", ENTITY)
            .unwrap();
        let _: EntityResponse = actor
            .ask(EntityMsg::GetState, Duration::from_secs(5))
            .await
            .unwrap();
        let table = state
            .registry
            .read()
            .unwrap()
            .get_table_live(&tenant, "Case")
            .unwrap();
        let competing_system = ActorSystem::new("system-one-competing-writer");
        let mutation = match seed % 3 {
            0 => Mutation::Bump(actor),
            1 => {
                let competing = competing_system.spawn(
                    EntityActor::with_persistence(
                        "Case",
                        ENTITY,
                        table.clone(),
                        json!({}),
                        BoxedEventStore::new(sim.clone()),
                        BackendLabel::Sim,
                    )
                    .with_tenant(TENANT),
                    format!("competitor-{seed}"),
                );
                let _: EntityResponse = competing
                    .ask(EntityMsg::GetState, Duration::from_secs(5))
                    .await
                    .unwrap();
                Mutation::Bump(competing)
            }
            _ => Mutation::Reload(table),
        };
        *provider.mutation.lock().unwrap() = Some(mutation);
        let result = dispatch(&state, TENANT, "Escalate", json!({}), "stale-attempt").await;
        assert!(
            rejected(&result),
            "seed={seed}: stale evidence permitted transition"
        );
        assert_eq!(
            provider.calls(),
            1,
            "seed={seed}: inference was skipped or repeated"
        );
        let current = read(&state, TENANT).await;
        assert_eq!(
            current.state.status, "Open",
            "seed={seed}: speculative state leaked"
        );
        assert_eq!(
            current.state.fields["revision"].as_u64(),
            Some(u64::from(seed % 3 != 2)),
            "seed={seed}: committed competing mutation was not reflected"
        );
        let retry = dispatch(&state, TENANT, "Escalate", json!({}), "stale-attempt").await;
        assert!(
            rejected(&retry),
            "seed={seed}: retry authorized changed inputs/spec"
        );
        assert_eq!(
            provider.calls(),
            1,
            "seed={seed}: stale attempt was resampled"
        );
        assert!(
            sim.dump_journal(&format!("{TENANT}:Case:{ENTITY}"))
                .iter()
                .all(|event| event.event_type != "Escalate"),
            "seed={seed}: rejected transition entered journal"
        );
        let restarted_provider = TestProvider::new(seed, Answer::Failure);
        let restarted = server(sim, restarted_provider.clone());
        assert_eq!(read(&restarted, TENANT).await.state.status, "Open");
        assert_eq!(restarted_provider.calls(), 0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn identical_attempt_keys_do_not_share_answers_between_tenants() {
    let _context = install_deterministic_context(807);
    let provider = TestProvider::new(807, Answer::PerTenant);
    let state = server(SimEventStore::no_faults(807), provider.clone());
    assert!(
        dispatch(&state, TENANT, "Escalate", json!({}), "same-key")
            .await
            .unwrap()
            .success
    );
    assert!(rejected(
        &dispatch(&state, "tenant-b", "Escalate", json!({}), "same-key").await
    ));
    assert_eq!(provider.calls(), 2);
    assert_eq!(read(&state, TENANT).await.state.status, "Escalated");
    assert_eq!(read(&state, "tenant-b").await.state.status, "Open");
}

#[tokio::test(flavor = "current_thread")]
async fn completed_attempt_cannot_be_reused_with_different_params_or_action() {
    let _context = install_deterministic_context(808);
    let provider = TestProvider::new(808, Answer::Positive);
    let state = server(SimEventStore::no_faults(808), provider.clone());
    assert!(
        dispatch(&state, TENANT, "Escalate", json!({}), "attempt")
            .await
            .unwrap()
            .success
    );
    let changed_params = dispatch(
        &state,
        TENANT,
        "Escalate",
        json!({"Messages":"different"}),
        "attempt",
    )
    .await;
    assert!(
        rejected(&changed_params),
        "completed key accepted different parameters"
    );
    let changed_action = dispatch(&state, TENANT, "Bump", json!({}), "attempt").await;
    assert!(
        rejected(&changed_action),
        "completed key accepted a different action"
    );
    assert_eq!(provider.calls(), 1);
}
