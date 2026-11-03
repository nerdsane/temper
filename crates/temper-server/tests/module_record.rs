//! The context a module actually receives after a record takes large parameters.

use serde_json::{Value, json};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::{
    ServerState, registry::SpecRegistry, request_context::AgentContext, state::DispatchCommand,
};
use temper_spec::csdl::parse_csdl;

const SPEC: &str = r#"
[automaton]
name = "Record"
states = ["Open", "Read"]
initial = "Open"

[[action]]
name = "Note"
kind = "input"
from = ["Open"]
to = "Open"

[[action]]
name = "Inspect"
kind = "input"
from = ["Open"]
to = "Open"

[[action.triggers]]
name = "reader"
kind = "wasm"
module = "reader"
on_success = "Received"

[[action]]
name = "Received"
kind = "input"
from = ["Open"]
to = "Read"
"#;

const CSDL: &str = r#"
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Record">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Records" EntityType="Test.Record"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

/// Return the actual input bytes as callback params, without interpreting them.
/// The callback's journal event lets the test inspect what crossed the guest ABI.
const READER: &str = r#"(module
    (import "env" "host_set_result" (func $result (param i32 i32)))
    (memory (export "memory") 1)
    (data (i32.const 0) "{\22success\22:true,\22params\22:")
    (func (export "run") (param $ptr i32) (param $len i32) (result i32)
        (local $out i32)
        (local.set $out
            (i32.mul
                (memory.grow
                    (i32.div_u (i32.add (local.get $len) (i32.const 65561)) (i32.const 65536)))
                (i32.const 65536)))
        (memory.copy (local.get $out) (i32.const 0) (i32.const 25))
        (memory.copy (i32.add (local.get $out) (i32.const 25)) (local.get $ptr) (local.get $len))
        (i32.store8 (i32.add (local.get $out) (i32.add (local.get $len) (i32.const 25))) (i32.const 125))
        (call $result (local.get $out) (i32.add (local.get $len) (i32.const 26)))
        i32.const 0))"#;

fn test_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL parses"),
        CSDL.into(),
        &[("Record", SPEC)],
    );
    let state = ServerState::from_registry(ActorSystem::new("module-record"), registry);
    let tenant = TenantId::default();
    let hash = state
        .wasm_engine
        .compile_and_cache(READER.as_bytes())
        .expect("reader compiles");
    state.register_wasm_module(&tenant, "reader", &hash);
    state
}

#[tokio::test]
async fn dispatch_gives_the_module_event_summaries_and_the_current_calls_params() {
    let state = test_state();
    let tenant = TenantId::default();
    let agent = AgentContext::system();
    // A person's moves: only the first changes the record, and each is
    // recorded. The same move a machine made would write no event after the
    // first, so history would hold one.
    let person = AgentContext {
        security_ctx: Some(temper_authz::SecurityContext::from_verified_jwt(
            "ann",
            temper_authz::PrincipalKind::Customer,
            None,
            None,
            None,
            None,
        )),
        ..AgentContext::default()
    };
    let sent = "x".repeat(1_000_000);
    for _ in 0..3 {
        let response = state
            .dispatch_tenant_action(
                &tenant,
                "Record",
                "one",
                "Note",
                json!({"description": sent}),
                &person,
            )
            .await
            .expect("Note dispatches");
        assert!(response.success, "{:?}", response.error);
    }
    let before = state
        .get_tenant_entity_state(&tenant, "Record", "one")
        .await
        .expect("record exists")
        .state;
    assert_eq!(
        before
            .events
            .iter()
            .filter(|event| event.params["description"] == sent)
            .count(),
        3
    );
    // Replace the live field with a small value; only history still carries
    // the three large descriptions.
    let params = json!({"description": "what this call was told"});
    let mut expected_fields = before.fields.clone();
    expected_fields["description"] = params["description"].clone();
    let response = state
        .dispatch(DispatchCommand {
            tenant: &tenant,
            entity_type: "Record",
            entity_id: "one",
            action: "Inspect",
            params: params.clone(),
            agent_ctx: &agent,
            await_integration: true,
            await_reactions: true,
        })
        .await
        .expect("Inspect and its callback dispatch");
    assert!(response.success, "{:?}", response.error);
    assert_eq!(response.state.status, "Read");
    let received = &response
        .state
        .events
        .back()
        .expect("callback is journaled")
        .params;
    assert_eq!(received["trigger_params"], params);
    let record = &received["entity_state"];
    assert!(
        record.to_string().len() < 10_000,
        "module received historical payloads"
    );
    let events = record["events"]
        .as_array()
        .expect("module sees event history");
    let actual: Vec<_> = events
        .iter()
        .filter(|event| event["action"] == "Note")
        .cloned()
        .collect();
    let expected: Vec<Value> = before
        .events
        .iter()
        .filter(|event| event.action == "Note")
        .map(|event| {
            json!({
                "action": event.action,
                "from_status": event.from_status,
                "to_status": event.to_status,
                "timestamp": event.timestamp,
            })
        })
        .collect();
    assert_eq!(
        actual, expected,
        "the guest receives exactly each move's action, statuses and time"
    );
    assert!(events.iter().all(|event| event.get("params").is_none()));

    assert_eq!(record["entity_type"], before.entity_type);
    assert_eq!(record["entity_id"], before.entity_id);
    assert_eq!(record["fields"], expected_fields);
}
