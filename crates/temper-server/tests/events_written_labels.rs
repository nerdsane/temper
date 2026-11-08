//! The events counter says where each written event came from: its action, and
//! whether a request, a timer or a reaction made the move.
//! Its own binary installs the meter before runtime instruments initialize.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use serde_json::json;
use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const TENANT: &str = "alpha";

/// `Ask` is a request, `Mark` the reaction it triggers, `Later` the timer it
/// arms.
const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Open", "Asked"]
initial = "Open"
allow_indefinite_states = ["Open", "Asked"]

[[state]]
name = "marks"
type = "counter"
initial = 0

[[state]]
name = "laters"
type = "counter"
initial = 0

[[action]]
name = "Ask"
kind = "input"
from = ["Open"]
to = "Asked"
effect = ["schedule('Later', 30)"]

[[action.triggers]]
name = "marks"
kind = "entity"
target_entity = "Job"
target_action = "Mark"
resolve_target = { kind = "same_id" }

[[action]]
name = "Mark"
kind = "input"
from = ["Asked"]
effect = ["marks += 1"]

[[action]]
name = "Later"
kind = "input"
from = ["Asked"]
effect = ["laters += 1"]
"#;

const CSDL: &str = r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Test.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// The counter's value by action and how the move came about, for the labels
/// the counter must carry.
fn written(
    provider: &SdkMeterProvider,
    exporter: &InMemoryMetricExporter,
) -> BTreeMap<(String, String), u64> {
    provider.force_flush().unwrap();
    let mut counted = BTreeMap::new();
    for batch in exporter.get_finished_metrics().unwrap() {
        for scope in batch.scope_metrics {
            for metric in scope
                .metrics
                .into_iter()
                .filter(|metric| metric.name == "temper_events_written_total")
            {
                let sum = metric
                    .data
                    .as_any()
                    .downcast_ref::<Sum<u64>>()
                    .expect("counter");
                for point in &sum.data_points {
                    let attrs: BTreeMap<_, _> = point
                        .attributes
                        .iter()
                        .map(|kv| (kv.key.as_str(), kv.value.as_str().to_string()))
                        .collect();
                    assert_eq!(attrs.len(), 4, "{attrs:?}");
                    assert_eq!(attrs["tenant"], TENANT);
                    assert_eq!(attrs["entity_type"], "Job");
                    counted.insert((attrs["action"].clone(), attrs["via"].clone()), point.value);
                }
            }
        }
    }
    counted
}

#[tokio::test(start_paused = true)]
async fn each_written_event_is_counted_by_its_action_and_how_it_came_about() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_resource(
            Resource::builder_empty()
                .with_attribute(KeyValue::new("app_id", "test-app"))
                .build(),
        )
        .with_periodic_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());

    let clock = Arc::new(process_clock::ProcessClock::new());
    clock.begin();
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(7)));
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        TENANT,
        parse_csdl(CSDL).unwrap(),
        CSDL.to_string(),
        &[("Job", JOB)],
    );
    let store = SimEventStore::no_faults(7);
    let mut state = ServerState::from_registry(ActorSystem::new("events-written"), registry);
    let temp = tempfile::tempdir().unwrap();
    state.data_dir = temp.path().to_path_buf();
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .unwrap();
    state.rebuild_reaction_dispatcher();
    let walk = state.start_runtime().await.expect("the first start");
    walk.await.unwrap();

    let tenant = TenantId::new(TENANT);
    state
        .get_or_create_tenant_entity(&tenant, "Job", "one", json!({}))
        .await
        .unwrap();
    let agent = AgentContext {
        security_ctx: Some(SecurityContext::from_verified_jwt(
            "hook-1",
            PrincipalKind::Agent,
            Some("hook"),
            Some("owner-1"),
            None,
            None,
        )),
        agent_id: Some("hook-1".into()),
        ..Default::default()
    };
    let asked = state
        .dispatch_tenant_action(&tenant, "Job", "one", "Ask", json!({}), &agent)
        .await
        .unwrap();
    assert!(asked.success, "{asked:?}");

    tokio::time::sleep(Duration::from_secs(60)).await;
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }

    let journal: Vec<String> = store
        .dump_journal(&format!("{TENANT}:Job:one"))
        .iter()
        .map(|event| event.event_type.clone())
        .collect();
    for action in ["Ask", "Mark", "Later"] {
        assert!(
            journal.iter().any(|written| written == action),
            "{action} was written: {journal:?}"
        );
    }
    let counted = written(&provider, &exporter);
    for (action, via) in [("Ask", "request"), ("Mark", "reaction"), ("Later", "timer")] {
        assert_eq!(
            counted.get(&(action.to_string(), via.to_string())),
            Some(&1),
            "{action} via {via}: {counted:?}"
        );
    }
    assert_eq!(
        counted.values().sum::<u64>(),
        u64::try_from(journal.len()).unwrap(),
        "every written event is counted once: {counted:?}"
    );
    clock.end(Duration::ZERO);
}
