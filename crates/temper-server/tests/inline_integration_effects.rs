//! Regression test for ARN-188.
//!
//! When `await_integration = true` and a transition emits BOTH a custom effect
//! (mapped to an inline integration that yields a callback response) AND a
//! `schedule` / `spawn` effect, the inline integration path must still run the
//! ORIGINAL transition's scheduled actions and spawn requests. The bug was an
//! early `return final_response` that skipped those steps, silently dropping the
//! scheduled timer and the child-entity spawn.

use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.InlineEffects" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Parent">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Child">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Parents" EntityType="Temper.InlineEffects.Parent"/>
        <EntitySet Name="Children" EntityType="Temper.InlineEffects.Child"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

// The `Trigger` transition emits, in one shot:
//   * an `http` adapter trigger  -> inline `AdapterSucceeded` callback
//   * a `schedule` effect        -> fires `Tick` on the parent (Done -> Ticked)
//   * a `spawn` effect           -> creates a `Child` and runs `Init` (New -> Initialized)
//
// Under `await_integration = true`, the inline adapter callback moves the parent
// to `Done`. The scheduled `Tick` and the spawned `Child` belong to the ORIGINAL
// `Trigger` response and must still be applied.
const PARENT_SPEC: &str = r#"
[automaton]
name = "Parent"
states = ["Idle", "Pending", "Done", "Ticked", "Failed"]
initial = "Idle"

[[action]]
name = "Trigger"
kind = "input"
from = ["Idle"]
to = "Pending"
params = ["child_id"]
effect = ["schedule('Tick', 0)", "spawn('Child', 'Init', child_ref, params.child_id)"]

[[action.triggers]]
name = "adapter_call"
kind = "adapter"
adapter = "http"
on_success = "AdapterSucceeded"
on_failure = "AdapterFailed"
config = { method = "POST", url = "{url}/execute" }

[[action]]
name = "AdapterSucceeded"
kind = "input"
from = ["Pending"]
to = "Done"
params = ["result"]

[[action]]
name = "AdapterFailed"
kind = "input"
from = ["Pending"]
to = "Failed"
params = ["error_message"]

[[action]]
name = "Tick"
kind = "input"
from = ["Done"]
to = "Ticked"
"#;

const CHILD_SPEC: &str = r#"
[automaton]
name = "Child"
states = ["New", "Initialized"]
initial = "New"

[[action]]
name = "Init"
kind = "input"
from = ["New"]
to = "Initialized"
"#;

fn build_state(parent_spec: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Parent", parent_spec), ("Child", CHILD_SPEC)],
    );

    let system = ActorSystem::new("inline-integration-effects-test");
    ServerState::from_registry(system, registry)
}

/// Poll `get_tenant_entity_state` until the entity reaches `expected` status,
/// or the attempt budget is exhausted. Returns the last observed status.
async fn poll_status(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    expected: &str,
) -> String {
    let mut last = String::new();
    for _ in 0..100 {
        if let Ok(resp) = state
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
        {
            last = resp.state.status.clone();
            if last == expected {
                return last;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    last
}

#[tokio::test(flavor = "multi_thread")]
async fn inline_integration_still_runs_scheduled_and_spawn_effects() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/execute"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "callback_params": { "result": "ok" }
        })))
        .mount(&mock_server)
        .await;

    let spec = PARENT_SPEC.replace("{url}", &mock_server.uri());
    let state = build_state(&spec);
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "Parent",
            "parent-1",
            "Trigger",
            serde_json::json!({ "child_id": "child-1" }),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("Trigger should succeed");

    // The inline adapter callback must have been applied.
    assert!(response.success);
    assert_eq!(
        response.state.status, "Done",
        "inline adapter callback should move parent to Done"
    );

    // The scheduled action from the ORIGINAL transition must still fire.
    let parent_status = poll_status(&state, &tenant, "Parent", "parent-1", "Ticked").await;
    assert_eq!(
        parent_status, "Ticked",
        "scheduled action `Tick` was dropped by the inline integration path (ARN-188)"
    );

    // The spawn request from the ORIGINAL transition must still create + init the child.
    let child_status = poll_status(&state, &tenant, "Child", "child-1", "Initialized").await;
    assert_eq!(
        child_status, "Initialized",
        "spawn request for child entity was dropped by the inline integration path (ARN-188)"
    );
}

/// The adapter's answer, given once the request's own action has committed;
/// from then on the store takes no write to the parent.
struct AnswerAfterTheStoreStops {
    store: SimEventStore,
}

impl Respond for AnswerAfterTheStoreStops {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.store
            .fail_next_appends("default:Parent:parent-1", usize::MAX);
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "callback_params": { "result": "ok" }
        }))
    }
}

/// When the store does not take an inline integration's answer, the request's
/// own action has already committed. The caller is answered committed with
/// the refused answer named (ADR-0184), never that the store is unavailable:
/// that answer (503, Retry-After) asks the caller to send the action again,
/// and the action would apply twice.
#[tokio::test(flavor = "multi_thread")]
async fn an_answer_the_store_does_not_take_does_not_ask_for_the_action_again() {
    let mock_server = MockServer::start().await;
    let store = SimEventStore::no_faults(41);
    Mock::given(method("POST"))
        .and(path("/execute"))
        .respond_with(AnswerAfterTheStoreStops {
            store: store.clone(),
        })
        .mount(&mock_server)
        .await;

    let spec = PARENT_SPEC.replace("{url}", &mock_server.uri());
    let mut state = build_state(&spec);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "Parent",
            "parent-1",
            "Trigger",
            serde_json::json!({ "child_id": "child-1" }),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("Trigger should dispatch");

    let journal = store
        .read_events("default:Parent:parent-1", 0)
        .await
        .expect("the journal reads");
    let actions: Vec<&str> = journal.iter().map(|e| e.event_type.as_str()).collect();
    assert!(
        actions.contains(&"Trigger") && !actions.contains(&"AdapterSucceeded"),
        "the action committed and its answer did not: {actions:?}"
    );
    assert!(response.success, "the action committed: {response:?}");
    assert!(
        response
            .trigger_failures
            .iter()
            .any(|failure| failure.starts_with("answer refused: ")),
        "the answer the store did not take is named: {:?}",
        response.trigger_failures
    );
    assert!(
        !response.store_unavailable,
        "the action committed, so the caller must not be asked to send it again: {:?}",
        response.error
    );
}
