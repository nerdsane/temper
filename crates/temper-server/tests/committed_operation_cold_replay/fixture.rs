//! Published-review #523: committed-prefix provenance is not necessarily the
//! original post-integration logical reply. Uses real router/actors/WASM and
//! a fresh server/cache over the journal, never edited response-cache entries.
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::{
    ServerState, StorageStack, build_router,
    registry::{EntityVerificationResult, SpecRegistry, VerificationStatus},
};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

pub(super) const CSDL: &str = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Work"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Test.Work"/></Action><Action Name="Complete" IsBound="true"><Parameter Name="bindingParameter" Type="Test.Work"/></Action><Action Name="Archive" IsBound="true"><Parameter Name="bindingParameter" Type="Test.Work"/></Action><EntityContainer Name="Container"><EntitySet Name="Works" EntityType="Test.Work"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;
const TRIGGER: &str = r#"
[[action.triggers]]
name="finish"
kind="wasm"
module="finish"
on_success="Complete"
"#;
pub(super) fn spec(integration: bool) -> String {
    format!(
        r#"
[automaton]
name="Work"
states=["Idle","Running","Done","Archived"]
initial="Idle"
[[action]]
name="Start"
from=["Idle"]
to="Running"
{}
[[action]]
name="Complete"
from=["Running"]
to="Done"
[[action]]
name="Archive"
from=["Done"]
to="Archived"
"#,
        if integration { TRIGGER } else { "" }
    )
}
pub(super) fn server(store: &SimEventStore, integration: bool, label: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Work", &spec(integration))],
    );
    registry.set_verification_status(
        &TenantId::default(),
        "Work",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "test".into(),
        }),
    );
    let mut state = ServerState::from_registry(ActorSystem::new(label), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"permit(principal, action, resource) when { principal.id == "tester" };"#,
        )
        .unwrap();
    let payload = r#"{"success":true,"params":{}}"#;
    let data = payload
        .bytes()
        .map(|b| format!("\\{b:02x}"))
        .collect::<String>();
    let wat = format!(
        r#"(module
      (import "env" "host_set_result" (func $result (param i32 i32)))
      (memory (export "memory") 1)
      (data (i32.const 0) "{data}")
      (func (export "run") (param i32 i32) (result i32)
        i32.const 0 i32.const {} call $result i32.const 0))"#,
        payload.len()
    );
    let hash = state.wasm_engine.compile_and_cache(wat.as_bytes()).unwrap();
    state
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&TenantId::default(), "finish", &hash);
    state
}
pub(super) async fn request(
    state: &ServerState,
    action: Option<&str>,
    key: Option<&str>,
    awaited: bool,
) -> (StatusCode, Value) {
    let path = action.map_or_else(
        || "/tdata/Works".into(),
        |a| format!("/tdata/Works('one')/Test.{a}?await_integration={awaited}"),
    );
    let mut req = Request::post(path).header("content-type", "application/json");
    if let Some(key) = key {
        req = req.header("idempotency-key", key);
    }
    let mut req = req
        .body(Body::from(if action.is_none() {
            r#"{"Id":"one"}"#
        } else {
            "{}"
        }))
        .unwrap();
    req.extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_resolved_identity("tester", "test-agent", None),
        ));
    let resp = build_router(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    (
        status,
        serde_json::from_slice(&to_bytes(resp.into_body(), 4_000_000).await.unwrap()).unwrap(),
    )
}
pub(super) fn logical(body: &Value) -> Value {
    let mut body = body.clone();
    if let Some(o) = body.as_object_mut() {
        o.retain(|k, _| !k.starts_with("@odata."));
        o.remove("events_since_snapshot");
        o.remove("last_snapshot_sequence_nr");
    }
    body
}
pub(super) fn invocations(state: &ServerState) -> usize {
    state
        .entity_observe_log
        .lock()
        .unwrap()
        .values()
        .flatten()
        .filter(|e| e.entity_type == "Work" && e.event_name == "integration_complete")
        .count()
}
