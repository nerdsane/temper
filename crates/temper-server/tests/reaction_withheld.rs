//! Withheld reactions leave one warning, low-cardinality count and Authz trajectory.
//! Its own binary installs the meter before runtime instruments initialize.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use serde_json::{Value, json};
use temper_authz::{MoveCause, PrincipalKind, SecurityContext, TimerArm};
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::{TrajectoryEntry, TrajectorySource};
use temper_server::storage::TrajectorySink;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL: &str = r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
<EntityType Name="Ledger"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Test.Job"/><EntitySet Name="Ledgers" EntityType="Test.Ledger"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;
const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Open", "Noted"]
initial = "Open"
[[action]]
name = "Note"
kind = "input"
from = ["Open"]
to = "Noted"
[[action.triggers]]
name = "counts"
kind = "entity"
target_entity = "Ledger"
target_action = "Count"
resolve_target = { kind = "same_id" }
"#;
const LEDGER: &str = r#"
[automaton]
name = "Ledger"
states = ["Empty", "Counted"]
initial = "Empty"
[[action]]
name = "Count"
kind = "input"
from = ["Empty"]
to = "Counted"
"#;

#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);
impl Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl CapturedLog {
    fn withheld(&self) -> Vec<Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| event["fields"]["message"] == "reaction withheld")
            .collect()
    }
}

#[derive(Default)]
struct CapturedTrajectories(Mutex<Vec<TrajectoryEntry>>);
#[async_trait::async_trait]
impl TrajectorySink for CapturedTrajectories {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        self.0.lock().unwrap().push(entry.clone());
        Ok(())
    }
}

fn counts(provider: &SdkMeterProvider, exporter: &InMemoryMetricExporter) -> BTreeMap<String, u64> {
    provider.force_flush().unwrap();
    let mut counted = BTreeMap::new();
    for batch in exporter.get_finished_metrics().unwrap() {
        for scope in batch.scope_metrics {
            for metric in scope
                .metrics
                .into_iter()
                .filter(|m| m.name == "temper_reaction_withheld_total")
            {
                assert!(
                    batch.resource.iter().any(
                        |(key, value)| key.as_str() == "app_id" && value.as_str() == "test-app"
                    )
                );
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
                    assert_eq!(attrs.len(), 3, "identity belongs in logs, not labels");
                    assert_eq!(attrs["entity_type"], "Ledger");
                    assert_eq!(attrs["action"], "Count");
                    counted.insert(attrs["tenant"].clone(), point.value);
                }
            }
        }
    }
    counted
}

#[tokio::test(start_paused = true)]
async fn refused_reactions_are_visible_and_allowed_reactions_add_no_withheld_signal() {
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
    let logs = CapturedLog::default();
    let writer = logs.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || writer.clone())
            .finish(),
    )
    .unwrap();
    let mut expected = BTreeMap::<String, u64>::new();
    for seed in 0..16 {
        let clock = Arc::new(process_clock::ProcessClock::new());
        clock.begin();
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        for tenant_name in ["alpha", "beta"] {
            for deferred in [false, true] {
                for denied in [false, true] {
                    let mut registry = SpecRegistry::new();
                    let job = if seed % 2 == 0 {
                        JOB.to_string()
                    } else {
                        JOB.replace(
                            "kind = \"entity\"",
                            "kind = \"entity\"\nprincipal = \"counter\"",
                        )
                    };
                    registry.register_tenant(
                        tenant_name,
                        parse_csdl(CSDL).unwrap(),
                        CSDL.to_string(),
                        &[("Job", &job), ("Ledger", LEDGER)],
                    );
                    let store = SimEventStore::no_faults(seed);
                    let trajectories = Arc::new(CapturedTrajectories::default());
                    let mut storage = StorageStack::from_sim(store.clone(), None);
                    storage.trajectory = Some(trajectories.clone());
                    let mut state =
                        ServerState::from_registry(ActorSystem::new("reaction-withheld"), registry);
                    let temp = tempfile::tempdir().unwrap();
                    state.data_dir = temp.path().to_path_buf();
                    state.set_storage_stack(storage);
                    state.authz.reload_tenant_policies(tenant_name, r#"
                        permit(principal, action, resource);
                        forbid(principal, action == Action::"Count", resource) when { resource.id == "denied" };
                    "#).unwrap();
                    state.rebuild_reaction_dispatcher();
                    let tenant = TenantId::new(tenant_name);
                    let id = if denied { "denied" } else { "allowed" };
                    let journal = format!("{tenant_name}:Ledger:{id}");
                    if deferred {
                        store.fail_next_reads(&journal, usize::MAX);
                    }
                    let mut security = SecurityContext::from_verified_jwt(
                        "hook-1",
                        PrincipalKind::Agent,
                        Some("hook"),
                        Some("owner-1"),
                        None,
                        None,
                    );
                    if seed % 4 >= 2 {
                        security.cause = Some(MoveCause::timer_fire(TimerArm {
                            armed_by: "hook-1".into(),
                            state_timer_of: None,
                        }));
                    }
                    let agent = AgentContext {
                        security_ctx: Some(security),
                        agent_id: Some("hook-1".into()),
                        session_id: Some("session-1".into()),
                        intent: Some("count the note".into()),
                        ..Default::default()
                    };
                    let before_logs = logs.withheld().len();
                    let response = state
                        .dispatch_tenant_action(&tenant, "Job", id, "Note", json!({}), &agent)
                        .await
                        .unwrap();
                    assert!(response.success, "source must still commit: seed {seed}");
                    if deferred {
                        assert_eq!(
                            logs.withheld().len(),
                            before_logs,
                            "a read failure is not a refusal"
                        );
                        store.fail_next_reads(&journal, 0);
                    }
                    // The production retry and trajectory outbox run on the paused clock.
                    for _ in 0..2000 {
                        if trajectories
                            .0
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|e| e.action == "Count")
                        {
                            break;
                        }
                        tokio::time::advance(Duration::from_secs(10)).await;
                        tokio::task::yield_now().await;
                    }
                    let rows = trajectories.0.lock().unwrap();
                    let target: Vec<_> = rows.iter().filter(|e| e.action == "Count").collect();
                    assert_eq!(
                        target.len(),
                        1,
                        "one target trajectory: seed {seed}, deferred {deferred}, denied {denied}"
                    );
                    let row = target[0];
                    assert_eq!(
                        store
                            .dump_journal(&journal)
                            .iter()
                            .filter(|e| e.event_type == "Count")
                            .count(),
                        usize::from(!denied),
                        "only an allowed reaction runs"
                    );
                    assert_eq!(
                        (&*row.tenant, &*row.entity_type, &*row.entity_id),
                        (tenant_name, "Ledger", id)
                    );
                    assert_eq!(row.success, !denied);
                    if denied {
                        *expected.entry(tenant_name.into()).or_default() += 1;
                        assert_eq!(row.source, Some(TrajectorySource::Authz));
                        assert_eq!(row.authz_denied, Some(true));
                        let principal = if seed % 2 == 0 {
                            "hook-1"
                        } else {
                            "service:counter"
                        };
                        assert_eq!(row.agent_id.as_deref(), Some(principal));
                        assert_eq!(row.session_id.as_deref(), Some("session-1"));
                        assert_eq!(row.from_status.as_deref(), Some("Empty"));
                        assert!(row.to_status.is_none());
                        assert_eq!(row.denied_resource.as_deref(), Some("Ledger:denied"));
                        assert!(row.request_body.is_some());
                        assert_eq!(logs.withheld().len(), before_logs + 1);
                        let event = logs.withheld().pop().unwrap();
                        assert_eq!(event["level"], "WARN");
                        let fields = &event["fields"];
                        for (key, value) in [
                            ("tenant", tenant_name),
                            ("entity_type", "Ledger"),
                            ("entity_id", id),
                            ("action", "Count"),
                            ("rule", "Job:Note:counts"),
                            ("source_entity_type", "Job"),
                            ("source_entity_id", id),
                            ("source_action", "Note"),
                            ("principal_id", principal),
                            ("principal_kind", "Agent"),
                            ("via", if seed % 4 >= 2 { "timer" } else { "reaction" }),
                        ] {
                            assert_eq!(fields[key], value, "field {key}");
                        }
                        assert_eq!(fields["depth"], 1);
                        assert_eq!(
                            fields["acting_for"].as_str().unwrap().contains("owner-1"),
                            seed % 2 == 0
                        );
                        assert!(
                            row.error
                                .as_ref()
                                .unwrap()
                                .contains(fields["reason"].as_str().unwrap())
                        );
                    } else {
                        assert_eq!(logs.withheld().len(), before_logs);
                        assert_ne!(row.authz_denied, Some(true));
                    }
                    assert_eq!(
                        counts(&provider, &exporter),
                        expected,
                        "seed {seed}, deferred {deferred}, denied {denied}"
                    );
                }
            }
        }
        clock.end(Duration::ZERO);
    }
}
