//! Follow-ons the runtime starts by itself after a transition commits.
//!
//! A `schedule` that waits on the clock starts a fresh callback lineage, so a
//! periodic schedule (a cron, a token refresh) keeps cycling however many hops
//! its cycles spend. A scheduled follow-on of a keyed request gets a key
//! derived from the request's: a retry, while the follow-on waits or after it
//! ran, runs no second copy, and two requests that schedule the same action
//! both run.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::{AgentContext, MAX_CALLBACK_HOPS};
use temper_spec::csdl::parse_csdl;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.FollowOn" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Cron">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Run">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Bell">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Reminder">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Crons" EntityType="Temper.FollowOn.Cron"/>
        <EntitySet Name="Runs" EntityType="Temper.FollowOn.Run"/>
        <EntitySet Name="Bells" EntityType="Temper.FollowOn.Bell"/>
        <EntitySet Name="Reminders" EntityType="Temper.FollowOn.Reminder"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// The shape of a cron job: each cycle calls an integration, whose callback
/// spawns a run and schedules the next cycle one second later.
const CRON_SPEC: &str = r#"
[automaton]
name = "Cron"
states = ["Active", "Failed"]
initial = "Active"

[[state]]
name = "n"
type = "counter"
initial = 0

[[action]]
name = "Trigger"
kind = "input"
from = ["Active"]
effect = ["n += 1"]

[[action.triggers]]
name = "compute_next"
kind = "adapter"
adapter = "http"
on_success = "TriggerComplete"
on_failure = "TriggerFailed"
config = { method = "POST", url = "{url}/next" }

[[action]]
name = "TriggerComplete"
kind = "input"
from = ["Active"]
params = ["result"]
effect = ["schedule('Trigger', 1)", "spawn('Run', 'Start')"]

[[action]]
name = "TriggerFailed"
kind = "input"
from = ["Active"]
to = "Failed"
params = ["error_message"]
"#;

const RUN_SPEC: &str = r#"
[automaton]
name = "Run"
states = ["New", "Started"]
initial = "New"

[[action]]
name = "Start"
from = ["New"]
to = "Started"
"#;

/// Each press schedules one ring a second later.
const BELL_SPEC: &str = r#"
[automaton]
name = "Bell"
states = ["Ready"]
initial = "Ready"

[[state]]
name = "n"
type = "counter"
initial = 0

[[action]]
name = "Press"
from = ["Ready"]
effect = ["schedule('Ring', 1)"]

[[action]]
name = "Ring"
from = ["Ready"]
effect = ["n += 1"]
"#;

/// Schedules one action for two different moments.
const REMINDER_SPEC: &str = r#"
[automaton]
name = "Reminder"
states = ["Idle", "Set"]
initial = "Idle"

[[state]]
name = "n"
type = "counter"
initial = 0

[[action]]
name = "Set"
from = ["Idle"]
to = "Set"
effect = ["schedule('Ring', 1)", "schedule('Ring', 2)"]

[[action]]
name = "Ring"
from = ["Set"]
effect = ["n += 1"]
"#;

fn build_state(integration_url: &str) -> ServerState {
    build_state_with_bell(integration_url, BELL_SPEC)
}

fn build_state_with_bell(integration_url: &str, bell_spec: &str) -> ServerState {
    build_state_for_tenant("default", integration_url, bell_spec)
}

fn build_state_for_tenant(tenant: &str, integration_url: &str, bell_spec: &str) -> ServerState {
    let cron_spec = CRON_SPEC.replace("{url}", integration_url);
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        tenant,
        csdl,
        CSDL_XML.to_string(),
        &[
            ("Cron", cron_spec.as_str()),
            ("Run", RUN_SPEC),
            ("Bell", bell_spec),
            ("Reminder", REMINDER_SPEC),
        ],
    );
    ServerState::from_registry(ActorSystem::new("follow-on-lineage-test"), registry)
}

async fn counter(state: &ServerState, tenant: &TenantId, entity_type: &str, id: &str) -> usize {
    state
        .get_tenant_entity_state(tenant, entity_type, id)
        .await
        .map(|r| r.state.counters.get("n").copied().unwrap_or(0))
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clocked_schedule_keeps_cycling_past_the_callback_hop_budget() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/next"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "callback_params": { "result": "ok" }
        })))
        .mount(&mock_server)
        .await;
    let state = build_state(&mock_server.uri());
    let tenant = TenantId::default();
    // Start where a long-running cron would be after many cycles: two hops
    // left. Each cycle's integration callback spends one.
    let worn = AgentContext {
        callback_hops: MAX_CALLBACK_HOPS - 2,
        ..AgentContext::default()
    };
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Cron",
            "c1",
            "Trigger",
            serde_json::json!({}),
            &worn,
        )
        .await
        .expect("Trigger should commit");
    assert!(response.success);

    let mut cycles = 0;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cycles = counter(&state, &tenant, "Cron", "c1").await;
        if cycles >= 5 {
            break;
        }
    }
    assert!(cycles >= 5, "cron stopped after {cycles} cycles");

    // Every completed cycle's spawned run starts.
    let mut started = 0;
    for id in state.list_entity_ids(&tenant, "Run") {
        let run = state
            .get_tenant_entity_state(&tenant, "Run", &id)
            .await
            .expect("spawned run should load");
        if run.state.status == "Started" {
            started += 1;
        }
    }
    assert!(
        started >= cycles - 1,
        "{started} runs started for {cycles} cycles"
    );
}

async fn press(state: &ServerState, tenant: &TenantId, agent: &AgentContext) {
    let response = state
        .dispatch_tenant_action(tenant, "Bell", "b1", "Press", serde_json::json!({}), agent)
        .await
        .expect("Press should commit");
    assert!(response.success);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retried_request_rings_once() {
    let state = build_state("http://127.0.0.1:9");
    let tenant = TenantId::default();
    let agent = AgentContext {
        idempotency_key: Some(temper_server::idempotency::IdempotencyKey::Request(
            "press-once".into(),
        )),
        ..AgentContext::default()
    };
    // The retry is answered from the first request's reply, which carries
    // the same scheduled ring.
    press(&state, &tenant, &agent).await;
    press(&state, &tenant, &agent).await;

    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(counter(&state, &tenant, "Bell", "b1").await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_retried_after_its_ring_rang_rings_once() {
    let state = build_state("http://127.0.0.1:9");
    let tenant = TenantId::default();
    let agent = AgentContext {
        idempotency_key: Some(temper_server::idempotency::IdempotencyKey::Request(
            "press-once".into(),
        )),
        ..AgentContext::default()
    };
    press(&state, &tenant, &agent).await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(counter(&state, &tenant, "Bell", "b1").await, 1);

    // The ring has finished; the retry is still answered with the first
    // reply, which names the same scheduled ring.
    press(&state, &tenant, &agent).await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(counter(&state, &tenant, "Bell", "b1").await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_presses_in_the_same_second_ring_twice() {
    let state = build_state("http://127.0.0.1:9");
    let tenant = TenantId::default();
    press(&state, &tenant, &AgentContext::default()).await;
    press(&state, &tenant, &AgentContext::default()).await;

    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(counter(&state, &tenant, "Bell", "b1").await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_action_scheduled_for_two_moments_fires_at_both() {
    let state = build_state("http://127.0.0.1:9");
    let tenant = TenantId::default();
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Reminder",
            "r1",
            "Set",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("Set should commit");
    assert!(response.success);

    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert_eq!(counter(&state, &tenant, "Reminder", "r1").await, 2);
}

/// The scheduled ring calls an HTTP adapter, so deduplicating its counter
/// alone cannot hide replayed post-dispatch effects.
async fn http_bell() -> (ServerState, MockServer) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ring"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "callback_params": {}
        })))
        .mount(&server)
        .await;
    let bell_spec = format!(
        r#"{BELL_SPEC}
[[action.triggers]]
name = "ring_http"
kind = "adapter"
adapter = "http"
on_success = "RingComplete"
on_failure = "RingFailed"
config = {{ method = "POST", url = "{}/ring" }}

[[action]]
name = "RingComplete"
from = ["Ready"]

[[action]]
name = "RingFailed"
from = ["Ready"]
"#,
        server.uri()
    );
    (build_state_with_bell(&server.uri(), &bell_spec), server)
}

async fn assert_one_http_ring(state: &ServerState, server: &MockServer) {
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(counter(state, &TenantId::default(), "Bell", "b1").await, 1);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "a cached scheduled reply must not launch its HTTP adapter again"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_retried_while_its_ring_waits_calls_http_once() {
    let (state, server) = http_bell().await;
    let tenant = TenantId::default();
    let agent = AgentContext {
        idempotency_key: Some(temper_server::idempotency::IdempotencyKey::Request(
            "press-once".into(),
        )),
        ..AgentContext::default()
    };
    press(&state, &tenant, &agent).await;
    press(&state, &tenant, &agent).await;
    assert_one_http_ring(&state, &server).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_retried_after_its_ring_calls_http_once() {
    let (state, server) = http_bell().await;
    let tenant = TenantId::default();
    let agent = AgentContext {
        idempotency_key: Some(temper_server::idempotency::IdempotencyKey::Request(
            "press-once".into(),
        )),
        ..AgentContext::default()
    };
    press(&state, &tenant, &agent).await;
    assert_one_http_ring(&state, &server).await;

    press(&state, &tenant, &agent).await;
    assert_one_http_ring(&state, &server).await;

    // Also exercise duplicate delivery of the scheduled child itself, even
    // when the parent retry no longer starts a second timer.
    let child = AgentContext {
        idempotency_key: Some(temper_server::idempotency::IdempotencyKey::FollowUp(
            temper_server::idempotency::derived_idempotency_key(
                "press-once",
                "scheduled:Ring",
                "0",
            ),
        )),
        ..AgentContext::default()
    };
    let response = state
        .dispatch_tenant_action(&tenant, "Bell", "b1", "Ring", serde_json::json!({}), &child)
        .await
        .expect("cached Ring should succeed");
    assert!(response.success);
    assert_one_http_ring(&state, &server).await;
}

#[path = "follow_on_lineage/dst.rs"]
mod dst;

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

#[path = "follow_on_lineage/restart.rs"]
mod restart;
