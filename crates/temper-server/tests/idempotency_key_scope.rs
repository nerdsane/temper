//! An `Idempotency-Key` answers only a retry of the request it was first used
//! for, and that request's post-dispatch effects run once.
//!
//! A different action or body sent with a key the entity already recorded is
//! refused: it does not report the earlier request's success as its own or run
//! its own reactions. A retry that arrives while the first request is still
//! running its effects waits for it and does not run them again.

use std::sync::Arc;
use std::time::Duration;

use temper_jit::table::Refusal;
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::idempotency::IdempotencyCache;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_spec::csdl::parse_csdl;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SERVICE: &str = "key-scope-test";

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.KeyScope" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Ticket">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Tally">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Job">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Clock">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Tickets" EntityType="Temper.KeyScope.Ticket"/>
        <EntitySet Name="Tallies" EntityType="Temper.KeyScope.Tally"/>
        <EntitySet Name="Jobs" EntityType="Temper.KeyScope.Job"/>
        <EntitySet Name="Clocks" EntityType="Temper.KeyScope.Clock"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Close` is valid only from `Open`, and bumps the Tally of the same id.
const TICKET_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Draft", "Open", "Closed"]
initial = "Draft"

[[state]]
name = "notes"
type = "counter"
initial = 0

[[action]]
name = "Note"
kind = "input"
from = ["Draft"]
to = "Draft"
params = ["text"]
effect = ["notes += 1"]

[[action]]
name = "Open"
kind = "input"
from = ["Draft"]
to = "Open"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action.triggers]]
name = "close_bumps_tally"
kind = "entity"
target_entity = "Tally"
target_action = "Bump"
resolve_target = { kind = "same_id" }
"#;

const TALLY_IOA: &str = r#"
[automaton]
name = "Tally"
states = ["Counting"]
initial = "Counting"

[[state]]
name = "bumped"
type = "counter"
initial = 0

[[action]]
name = "Bump"
kind = "input"
from = ["Counting"]
effect = ["bumped += 1"]
"#;

/// `Start` calls an outside service inline and moves to `Started` on its answer.
const JOB_IOA: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Pending", "Started", "Failed"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Pending"

[[action.triggers]]
name = "notify_outside"
kind = "adapter"
adapter = "http"
on_success = "Answered"
on_failure = "Unanswered"
config = { method = "POST", url = "{url}/notify" }

[[action]]
name = "Answered"
kind = "input"
from = ["Pending"]
to = "Started"
params = ["result"]

[[action]]
name = "Unanswered"
kind = "input"
from = ["Pending"]
to = "Failed"
params = ["error_message"]
"#;

/// `Tick` schedules the next `Tick` until it has run six times.
const CLOCK_IOA: &str = r#"
[automaton]
name = "Clock"
states = ["Running"]
initial = "Running"

[[state]]
name = "ticks"
type = "counter"
initial = 0

[[action]]
name = "Tick"
kind = "input"
from = ["Running"]
guard = "ticks < 6"
effect = ["ticks += 1", "schedule('Tick', 0)"]
"#;

fn build_state(tenant: &str, job_ioa: &str, cache: Option<IdempotencyCache>) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            tenant,
            parse_csdl(CSDL_XML).expect("CSDL should parse"),
            CSDL_XML.to_string(),
            &[
                ("Ticket", TICKET_IOA),
                ("Tally", TALLY_IOA),
                ("Job", job_ioa),
                ("Clock", CLOCK_IOA),
            ],
        )
        .expect("tenant registration");
    let mut state = ServerState::from_registry(ActorSystem::new("key-scope"), registry);
    if let Some(cache) = cache {
        state.idempotency_cache = Arc::new(cache);
    }
    state
        .authz
        .reload_tenant_policies(
            tenant,
            r#"permit(principal == Agent::"service:key-scope-test", action, resource);"#,
        )
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
    state
}

fn caller(key: &str) -> AgentContext {
    AgentContext {
        idempotency_key: Some(temper_server::idempotency::IdempotencyKey::Request(
            key.to_string(),
        )),
        ..AgentContext::for_service(SERVICE)
    }
}

/// `(status, counter, events)` of record `t1`.
async fn read(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    counter: &str,
) -> (String, usize, usize) {
    let entity = state
        .get_tenant_entity_state(tenant, entity_type, "t1")
        .await
        .expect("record");
    let count = entity.state.counters.get(counter).copied().unwrap_or(0);
    (entity.state.status, count, entity.state.total_event_count)
}

/// How many recent events of record `t1` are `action`.
async fn events_of(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    action: &str,
) -> usize {
    let entity = state
        .get_tenant_entity_state(tenant, entity_type, "t1")
        .await
        .expect("record");
    entity
        .state
        .events
        .iter()
        .filter(|event| event.action == action)
        .count()
}

async fn key_reuse_for_another_action_is_refused(state: ServerState, tenant: TenantId) {
    let note = state
        .dispatch_tenant_action(
            &tenant,
            "Ticket",
            "t1",
            "Note",
            serde_json::json!({"text": "first"}),
            &caller("k1"),
        )
        .await
        .expect("Note dispatches");
    assert!(note.success, "{:?}", note.error);
    let before = read(&state, &tenant, "Ticket", "notes").await;

    let close = state
        .dispatch_tenant_action(
            &tenant,
            "Ticket",
            "t1",
            "Close",
            serde_json::json!({}),
            &caller("k1"),
        )
        .await
        .expect("Close dispatches");
    assert!(
        !close.success,
        "Close sent with Note's key reported success while the ticket is a Draft"
    );
    assert_eq!(
        close.refusal,
        Some(Refusal::IdempotencyKeyReused {
            action: "Note".to_string()
        }),
        "the refusal names the key's reuse and the action it recorded: {:?}",
        close.error
    );
    assert_eq!(
        read(&state, &tenant, "Ticket", "notes").await,
        before,
        "the ticket did not move"
    );
    assert!(
        !state.entity_exists(&tenant, "Tally", "t1"),
        "Close's reaction did not run"
    );

    // A true retry of Note is still answered as the first Note, applied once.
    let retry = state
        .dispatch_tenant_action(
            &tenant,
            "Ticket",
            "t1",
            "Note",
            serde_json::json!({"text": "first"}),
            &caller("k1"),
        )
        .await
        .expect("retried Note dispatches");
    assert!(retry.success, "{:?}", retry.error);
    assert_eq!(read(&state, &tenant, "Ticket", "notes").await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_used_for_one_action_is_refused_for_another_while_its_reply_is_cached() {
    let tenant = TenantId::new("tickets");
    let state = build_state("tickets", JOB_IOA, None);
    key_reuse_for_another_action_is_refused(state, tenant).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_used_for_one_action_is_refused_for_another_once_its_reply_is_gone() {
    // A cache that holds nothing: only the entity's durable key set remains,
    // as after the reply expired, was evicted or the server restarted.
    let tenant = TenantId::new("tickets");
    let state = build_state(
        "tickets",
        JOB_IOA,
        Some(IdempotencyCache::with_budget_bytes(0)),
    );
    key_reuse_for_another_action_is_refused(state, tenant).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_used_with_one_body_is_refused_with_another() {
    let tenant = TenantId::new("tickets");
    let state = build_state("tickets", JOB_IOA, None);
    let who = caller("k1");
    let note = |text: &'static str| {
        state.dispatch_tenant_action(
            &tenant,
            "Ticket",
            "t1",
            "Note",
            serde_json::json!({"text": text}),
            &who,
        )
    };
    assert!(note("first").await.expect("Note dispatches").success);
    let other = note("second").await.expect("second Note dispatches");
    assert!(
        !other.success,
        "a different body under the same key was answered as the first"
    );
    assert_eq!(read(&state, &tenant, "Ticket", "notes").await.1, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_while_the_first_request_runs_its_effects_does_not_run_them_again() {
    let outside = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/notify"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "callback_params": { "result": "ok" } }))
                .set_delay(Duration::from_millis(800)),
        )
        .mount(&outside)
        .await;
    let tenant = TenantId::new("jobs");
    let state = build_state("jobs", &JOB_IOA.replace("{url}", &outside.uri()), None);
    let who = caller("start-1");
    let start = || {
        state.dispatch_tenant_action_ext(
            &tenant,
            "Job",
            "t1",
            "Start",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &who,
                await_integration: true,
                await_reactions: true,
            },
        )
    };

    let (first, retry) = tokio::join!(start(), async {
        // Arrives after Start committed, while its outside call is held open.
        tokio::time::sleep(Duration::from_millis(200)).await;
        start().await
    });
    let first = first.expect("Start dispatches");
    let retry = retry.expect("retried Start dispatches");

    let calls = outside.received_requests().await.expect("recorded").len();
    assert_eq!(
        calls, 1,
        "the outside service was called once for one Start"
    );
    assert!(first.success, "{:?}", first.error);
    assert!(retry.success, "{:?}", retry.error);
    // The inline callback has a key of its own, so it applies under the caller's key.
    assert_eq!(first.state.status, "Started");
    assert_eq!(
        retry.state.status, "Started",
        "the retry answers with the first reply"
    );
    assert_eq!(events_of(&state, &tenant, "Job", "Start").await, 1);
    assert_eq!(events_of(&state, &tenant, "Job", "Answered").await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_self_rescheduling_chain_keeps_one_key_length_every_cycle() {
    let tenant = TenantId::new("clocks");
    let state = build_state("clocks", JOB_IOA, None);
    let first = state
        .dispatch_tenant_action(
            &tenant,
            "Clock",
            "t1",
            "Tick",
            serde_json::json!({}),
            &caller("clock-start"),
        )
        .await
        .expect("Tick dispatches");
    assert!(first.success, "{:?}", first.error);

    let mut ticks = 0;
    for _ in 0..200 {
        ticks = read(&state, &tenant, "Clock", "ticks").await.1;
        if ticks == 6 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(ticks, 6, "each scheduled Tick ran under a key of its own");

    let entity = state
        .get_tenant_entity_state(&tenant, "Clock", "t1")
        .await
        .expect("record");
    let lengths: Vec<usize> = entity
        .state
        .processed_idempotency_keys
        .keys()
        .filter(|key| key.as_str() != "clock-start")
        .map(String::len)
        .collect();
    assert_eq!(lengths.len(), 5, "five scheduled cycles: {lengths:?}");
    assert!(
        lengths.windows(2).all(|pair| pair[0] == pair[1]),
        "a scheduled key does not grow from one cycle to the next: {lengths:?}"
    );
}
