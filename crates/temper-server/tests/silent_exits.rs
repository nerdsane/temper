//! A silent exit (ADR-0056 Sub-Decision 3) is a trigger whose module succeeds
//! but sends no answer: the record waits for a move that will not come.
//!
//! A `Note` is `Live` and stays there: `Use` triggers a module, and the move it
//! answers, `Noted`, keeps the status. That answer is a move, so it is not a
//! silent exit, and neither is a module that says it has none (`"empty":
//! true`). A module that names no move and says nothing is one, whether the
//! caller waits for the trigger or not. Each case runs in its own tenant, and
//! `temper_integration_silent_exit_total` is read back per tenant from the
//! metrics a collector receives.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_spec::csdl::parse_csdl;

const NOTE_IOA: &str = r#"
[automaton]
name = "Note"
states = ["Live"]
initial = "Live"

[[action]]
name = "Use"
kind = "input"
from = ["Live"]
hint = "Asks the module about the note; the note stays Live."

[[action.triggers]]
name = "look"
kind = "wasm"
module = "look"

[[action]]
name = "Noted"
kind = "internal"
from = ["Live"]
hint = "The module's answer; it keeps the note Live."
"#;

const NOTE_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Note" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Note">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Notes" EntityType="Temper.Note.Note"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const SILENT_EXITS: &str = "temper_integration_silent_exit_total";

/// The meter the runtime metrics bind to on first use, so every test in this
/// binary shares it and tells its counts apart by tenant.
fn metrics() -> &'static (SdkMeterProvider, InMemoryMetricExporter) {
    static METRICS: OnceLock<(SdkMeterProvider, InMemoryMetricExporter)> = OnceLock::new();
    METRICS.get_or_init(|| {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        opentelemetry::global::set_meter_provider(provider.clone());
        (provider, exporter)
    })
}

/// Silent exits counted so far, by tenant.
fn silent_exits() -> BTreeMap<String, u64> {
    let (provider, exporter) = metrics();
    provider.force_flush().expect("flush metrics");
    let mut counted = BTreeMap::new();
    for batch in exporter.get_finished_metrics().expect("metrics") {
        for scope in &batch.scope_metrics {
            for metric in scope.metrics.iter().filter(|m| m.name == SILENT_EXITS) {
                let sum = metric
                    .data
                    .as_any()
                    .downcast_ref::<Sum<u64>>()
                    .expect("a counter");
                for point in &sum.data_points {
                    let tenant = point
                        .attributes
                        .iter()
                        .find(|kv| kv.key.as_str() == "tenant")
                        .map(|kv| kv.value.as_str().to_string())
                        .unwrap_or_default();
                    counted.insert(tenant, point.value);
                }
            }
        }
    }
    counted
}

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

/// Use a note in tenant `name`, its module answering `result`, waiting for
/// the trigger when `wait` is true; then the silent exits counted for `name`,
/// once the module's answer has had time to land.
async fn silent_exits_after_use(name: &str, result: serde_json::Value, wait: bool) -> u64 {
    metrics();
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(NOTE_CSDL_XML).expect("CSDL parses");
    registry.register_tenant(name, csdl, NOTE_CSDL_XML.to_string(), &[("Note", NOTE_IOA)]);
    let state = ServerState::from_registry(ActorSystem::new(name), registry);
    let tenant = TenantId::new(name);
    let hash = state
        .wasm_engine
        .compile_and_cache(answering(&result).as_bytes())
        .expect("the module compiles");
    state.register_wasm_module(&tenant, "look", &hash);

    let used = state
        .dispatch_tenant_action_ext(
            &tenant,
            "Note",
            "n1",
            "Use",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &AgentContext::system(),
                await_integration: wait,
                await_reactions: true,
            },
        )
        .await
        .expect("Use dispatches");
    assert!(used.success, "Use is taken: {:?}", used.error);

    let note = state
        .get_tenant_entity_state(&tenant, "Note", "n1")
        .await
        .expect("the note exists")
        .state;
    assert_eq!(note.status, "Live", "the note stays Live");

    // A Use that waited has run its trigger. One that did not runs it in the
    // background: wait until it is counted, or long enough that it would be.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let counted = silent_exits().get(name).copied().unwrap_or(0);
        if wait || counted > 0 || tokio::time::Instant::now() >= deadline {
            return counted;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_keeps_the_status_is_not_a_silent_exit() {
    let counted =
        silent_exits_after_use("keeps", serde_json::json!({ "action": "Noted" }), true).await;
    assert_eq!(counted, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_answer_is_not_a_silent_exit() {
    let counted = silent_exits_after_use("empty", serde_json::json!({ "empty": true }), true).await;
    assert_eq!(counted, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_that_answers_no_move_is_a_silent_exit() {
    let counted = silent_exits_after_use("silent", serde_json::json!({ "params": {} }), true).await;
    assert_eq!(counted, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_module_that_answers_no_move_in_the_background_is_a_silent_exit() {
    let counted =
        silent_exits_after_use("silent-bg", serde_json::json!({ "params": {} }), false).await;
    assert_eq!(counted, 1);
}
