//! A move's declared answer is made as whoever made the move.
//!
//! A module's answer, an adapter's answer and the compensating move after a
//! failed integration are the spec's own consequences of a move, as a
//! reaction and a timer's move are. Each is made as whoever made that move,
//! so the reactions after it are decided by Cedar for that person.
//!
//! Here Cedar lets only `ann` move a record. `ann` makes the first move, the
//! integration answers in the background, and the answer's reaction `Tell`
//! must take the record to `Told`. Made as a service instead, the reaction is
//! refused and the record stays where the answer left it.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ONLY_ANN: &str = r#"permit(principal, action, resource) when { principal.id == "ann" };"#;

const ERRAND_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Errand" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Errand">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Errands" EntityType="Temper.Errand.Errand"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// An errand whose `Ask` calls `trigger`, answered by `Answer` or `Miss`,
/// with `extra` actions; both answers set off `Tell`.
fn errand(trigger: &str, extra: &str) -> String {
    format!(
        r#"
[automaton]
name = "Errand"
states = ["Idle", "Asked", "Answered", "Missed", "Failed", "Told"]
initial = "Idle"

[[action]]
name = "Ask"
kind = "input"
from = ["Idle"]
to = "Asked"

{trigger}

[[action]]
name = "Answer"
kind = "input"
from = ["Asked"]
to = "Answered"

[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "Errand"
resolve_target = {{ kind = "same_id" }}
target_action = "Tell"

[[action]]
name = "Miss"
kind = "input"
from = ["Asked"]
to = "Missed"

[[action.triggers]]
name = "tell_missed"
kind = "entity"
target_entity = "Errand"
resolve_target = {{ kind = "same_id" }}
target_action = "Tell"

{extra}

[[action]]
name = "Tell"
kind = "input"
from = ["Answered", "Missed", "Failed"]
to = "Told"
"#
    )
}

const MODULE_TRIGGER: &str = r#"
[[action.triggers]]
name = "look"
kind = "wasm"
module = "look"
on_success = "Answer"
on_failure = "Miss"
"#;

/// A module that answers `result` whatever it is asked.
fn answering(result: &serde_json::Value) -> String {
    let payload = result.to_string();
    let data = payload
        .bytes()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    format!(
        r#"(module
        (import "env" "host_set_result" (func $result (param i32 i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{data}")
        (func (export "run") (param i32 i32) (result i32)
            i32.const 0 i32.const {} call $result i32.const 0))"#,
        payload.len()
    )
}

/// A Temper holding `spec`, whose Cedar lets only `ann` move a record.
fn temper(spec: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(ERRAND_CSDL_XML).expect("CSDL parses");
    registry.register_tenant(
        "default",
        csdl,
        ERRAND_CSDL_XML.to_string(),
        &[("Errand", spec)],
    );
    let state = ServerState::from_registry(ActorSystem::new("answers-act-for-the-asker"), registry);
    state
        .authz
        .reload_tenant_policies("default", ONLY_ANN)
        .expect("the policy loads");
    state.rebuild_reaction_dispatcher();
    state
}

/// `ann`, as the kernel knows her once her token is verified.
fn ann() -> AgentContext {
    AgentContext {
        security_ctx: Some(temper_authz::SecurityContext::from_resolved_identity(
            "ann", "person", None,
        )),
        agent_id: Some("ann".to_string()),
        ..AgentContext::default()
    }
}

/// The status the errand settles in after `ann` asks: `Told`, or wherever
/// it was left once nothing has moved it for a while.
async fn settles_after_ann_asks(state: &ServerState, id: &str) -> String {
    let tenant = TenantId::default();
    let asker = AgentContext {
        idempotency_key: Some(temper_server::idempotency::IdempotencyKey::Request(
            id.into(),
        )),
        ..ann()
    };
    let asked = state
        .dispatch_tenant_action(&tenant, "Errand", id, "Ask", serde_json::json!({}), &asker)
        .await
        .expect("Ask dispatches");
    assert!(asked.success, "ann may ask: {:?}", asked.error);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = state
            .get_tenant_entity_state(&tenant, "Errand", id)
            .await
            .expect("the record exists")
            .state
            .status;
        if status == "Told" || tokio::time::Instant::now() >= deadline {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn with_module(state: &ServerState, result: serde_json::Value) {
    let hash = state
        .wasm_engine
        .compile_and_cache(answering(&result).as_bytes())
        .expect("the module compiles");
    state.register_wasm_module(&TenantId::default(), "look", &hash);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_answer_is_made_as_the_asker() {
    let state = temper(&errand(MODULE_TRIGGER, ""));
    with_module(&state, serde_json::json!({ "params": {} }));
    assert_eq!(settles_after_ann_asks(&state, "answered").await, "Told");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_failure_answer_is_made_as_the_asker() {
    let state = temper(&errand(MODULE_TRIGGER, ""));
    with_module(
        &state,
        serde_json::json!({ "success": false, "error": "no answer" }),
    );
    assert_eq!(settles_after_ann_asks(&state, "missed").await, "Told");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_adapter_answer_is_made_as_the_asker() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/execute"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;
    let trigger = format!(
        r#"
[[action.triggers]]
name = "look"
kind = "adapter"
adapter = "http"
on_success = "Answer"
config = {{ url = "{}/execute", method = "POST" }}
"#,
        server.uri()
    );
    let state = temper(&errand(&trigger, ""));
    assert_eq!(settles_after_ann_asks(&state, "adapted").await, "Told");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compensating_move_is_made_as_the_asker() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/execute"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;
    // No on_failure: the kernel compensates with the declared Fail.
    let trigger = format!(
        r#"
[[action.triggers]]
name = "look"
kind = "adapter"
adapter = "http"
on_success = "Answer"
config = {{ url = "{}/execute", method = "POST" }}
"#,
        server.uri()
    );
    let fail = r#"
[[action]]
name = "Fail"
kind = "input"
from = ["Asked"]
to = "Failed"
params = ["error_message"]

[[action.triggers]]
name = "tell_failed"
kind = "entity"
target_entity = "Errand"
resolve_target = { kind = "same_id" }
target_action = "Tell"
"#;
    let state = temper(&errand(&trigger, fail));
    assert_eq!(settles_after_ann_asks(&state, "compensated").await, "Told");
}
