//! An action's triggers are independent of each other, and a trigger that
//! fails does not undo the action that committed before it ran.
//!
//! `Place` has three triggers: `charge` calls a service that fails and has no
//! failure callback, `notify` calls one that answers, and `count` is a
//! reaction on the same order. However the action runs, `notify` and `count`
//! still happen, and the failed charge moves the order through its `Fail`
//! step (ADR-0152). A caller who waits is answered that `Place` committed,
//! with the failed trigger named beside it (ADR-0184).

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_spec::csdl::parse_csdl;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SERVICE: &str = "independent-triggers-test";
const TENANT: &str = "orders";

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Triggers" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Order">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Orders" EntityType="Temper.Triggers.Order"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const ORDER_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Placed", "Failed"]
initial = "Draft"

[[state]]
name = "notified"
type = "counter"
initial = 0

[[state]]
name = "counted"
type = "counter"
initial = 0

[[action]]
name = "Place"
kind = "input"
from = ["Draft"]
to = "Placed"

[[action.triggers]]
name = "charge"
kind = "adapter"
adapter = "http"
on_success = "Notified"
config = { method = "POST", url = "{url}/charge" }

[[action.triggers]]
name = "notify"
kind = "adapter"
adapter = "http"
on_success = "Notified"
config = { method = "POST", url = "{url}/notify" }

[[action.triggers]]
name = "count"
kind = "entity"
target_entity = "Order"
target_action = "Count"
resolve_target = { kind = "same_id" }

[[action]]
name = "Notified"
kind = "input"
from = ["Placed", "Failed"]
params = ["result"]
effect = ["notified += 1"]

[[action]]
name = "Count"
kind = "internal"
from = ["Placed", "Failed"]
effect = ["counted += 1"]

[[action]]
name = "Fail"
kind = "internal"
from = ["Placed"]
to = "Failed"
params = ["error", "error_message", "trigger_action"]
"#;

/// A service where `/charge` always fails and `/notify` answers.
async fn services() -> MockServer {
    let outside = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/charge"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&outside)
        .await;
    Mock::given(method("POST"))
        .and(path("/notify"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "callback_params": { "result": "sent" } })),
        )
        .mount(&outside)
        .await;
    outside
}

fn build_state(outside: &MockServer) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            TENANT,
            parse_csdl(CSDL_XML).expect("CSDL should parse"),
            CSDL_XML.to_string(),
            &[("Order", &ORDER_IOA.replace("{url}", &outside.uri()))],
        )
        .expect("tenant registration");
    let state = ServerState::from_registry(ActorSystem::new("independent-triggers"), registry);
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
    state
}

async fn place(
    state: &ServerState,
    id: &str,
    await_integration: bool,
) -> temper_server::EntityResponse {
    state
        .dispatch_tenant_action_ext(
            &TenantId::new(TENANT),
            "Order",
            id,
            "Place",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &AgentContext::for_service(SERVICE),
                await_integration,
                await_reactions: true,
            },
        )
        .await
        .expect("Place dispatches")
}

/// Whether the order reaches `status` with `notified` and `counted` at one
/// each, within ten seconds.
async fn settles(state: &ServerState, id: &str, status: &str) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let order = state
            .get_tenant_entity_state(&TenantId::new(TENANT), "Order", id)
            .await
            .expect("order")
            .state;
        let seen = (
            order.status.clone(),
            order.counters.get("notified").copied().unwrap_or(0),
            order.counters.get("counted").copied().unwrap_or(0),
        );
        if seen == (status.to_string(), 1, 1) {
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            return Err(format!("order {id} is {seen:?}, not ({status}, 1, 1)"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_caller_is_answered_committed_when_a_trigger_fails() {
    let outside = services().await;
    let state = build_state(&outside);

    let answer = place(&state, "o1", true).await;

    assert!(answer.success, "Place committed: {:?}", answer.error);
    assert_eq!(answer.error, None);
    assert_eq!(
        answer.trigger_failures.len(),
        1,
        "{:?}",
        answer.trigger_failures
    );
    assert!(
        answer.trigger_failures[0].starts_with("adapter integration failed"),
        "{:?}",
        answer.trigger_failures
    );
    settles(&state, "o1", "Failed").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_trigger_skips_none_after_it_in_the_background() {
    let outside = services().await;
    let state = build_state(&outside);

    let answer = place(&state, "o2", false).await;

    assert!(answer.success, "{:?}", answer.error);
    assert!(answer.trigger_failures.is_empty());
    settles(&state, "o2", "Failed").await.unwrap();
}
