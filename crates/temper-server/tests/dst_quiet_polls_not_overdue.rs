//! DST: a repeating timer that changes nothing keeps its row, and that is not
//! lateness.
//!
//! A poll whose fire changes nothing writes nothing, so its row stays at the
//! due time of the first look while the process goes on. The row is past due
//! all that time. `temper_timer_rows_overdue` and
//! `temper_state_timeout_armed_on_hydration_total{elapsed_bucket="overdue"}`
//! say a timer is late, and an operator is paged on them, so neither may count
//! such a row. Neither may they stop counting a real one.
//!
//! Workload, per seed: one tenant holds a `Poll`, watching every 15 seconds, and
//! a `Job`, a one-shot deadline an hour away. The poll looks a seeded number of
//! times, the process stops, and the next one boots after the deadline passed.
//!
//! Invariants:
//! - however many quiet looks ran, and though its row is past due, a poll is
//!   not on the gauge while a timer of this process waits for its next look;
//! - before a boot arms them no process holds either timer, so both rows are on
//!   the gauge: a row no process armed always counts;
//! - at boot the job's passed deadline is counted overdue, and the poll's look
//!   is counted as `repeating`, never overdue;
//! - after the boot the poll goes on and the gauge is empty again.
//!
//! Its own test binary, so the global meter provider is set before anything in
//! this process records a metric.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::DateTime;
use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::data::{Gauge, Sum};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use process_clock::ProcessClock;
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::StateTimerStore;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const SEEDS: u64 = 16;

/// `POLL`'s wait between looks.
const LOOK: Duration = Duration::from_secs(15);

/// `JOB`'s one deadline.
const DEADLINE: Duration = Duration::from_secs(3600);

/// Watches for ever: each `Look` resets the clock and changes nothing.
const POLL: &str = r#"
[automaton]
name = "Poll"
states = ["Idle", "Watching"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Watching"]

[[action]]
name = "Start"
from = ["Idle"]
to = "Watching"

[[action]]
name = "Look"
from = ["Watching"]

[[state_timeout]]
state = "Watching"
after_seconds = 15
on_timeout = "Look"
reset_on = ["Look"]
"#;

/// Waits once: `Expire` is the end of it.
const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Done"]

[[action]]
name = "Start"
from = ["Idle"]
to = "Waiting"

[[action]]
name = "Expire"
from = ["Waiting"]
to = "Done"

[[state_timeout]]
state = "Waiting"
after_seconds = 3600
on_timeout = "Expire"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Quiet" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Poll"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Quiet.Poll"/></Action>
<Action Name="Look" IsBound="true"><Parameter Name="bindingParameter" Type="Quiet.Poll"/></Action>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Quiet.Job"/></Action>
<Action Name="Expire" IsBound="true"><Parameter Name="bindingParameter" Type="Quiet.Job"/></Action>
<EntityContainer Name="Container">
<EntitySet Name="Polls" EntityType="Quiet.Poll"/>
<EntitySet Name="Jobs" EntityType="Quiet.Job"/>
</EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// Test-only blanket permit: this file checks timers, not authorization.
const PERMIT_ALL: &str = "permit(principal, action, resource);";

/// The meter every metric of this process records to, set before the first.
fn meter() -> &'static (SdkMeterProvider, InMemoryMetricExporter) {
    static METER: OnceLock<(SdkMeterProvider, InMemoryMetricExporter)> = OnceLock::new();
    METER.get_or_init(|| {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        opentelemetry::global::set_meter_provider(provider.clone());
        (provider, exporter)
    })
}

fn value_of(attributes: &[KeyValue], key: &str) -> String {
    attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().to_string())
        .unwrap_or_default()
}

/// `tenant`'s `temper_state_timeout_armed_on_hydration_total` by
/// `elapsed_bucket`, so far.
fn rearms_by_bucket(tenant: &str) -> BTreeMap<String, u64> {
    let (provider, exporter) = meter();
    provider.force_flush().expect("flush metrics");
    let mut counted = BTreeMap::new();
    // Cumulative sums: a later export holds the totals so far.
    for metrics in exporter.get_finished_metrics().expect("metrics") {
        for scope in &metrics.scope_metrics {
            for metric in scope
                .metrics
                .iter()
                .filter(|m| m.name == "temper_state_timeout_armed_on_hydration_total")
            {
                let sum = metric
                    .data
                    .as_any()
                    .downcast_ref::<Sum<u64>>()
                    .expect("a counter");
                for point in sum
                    .data_points
                    .iter()
                    .filter(|point| value_of(&point.attributes, "tenant") == tenant)
                {
                    counted.insert(value_of(&point.attributes, "elapsed_bucket"), point.value);
                }
            }
        }
    }
    counted
}

/// The newest value of gauge `name` for `tenant`'s state timers.
fn state_timer_gauge(name: &str, tenant: &str) -> Option<u64> {
    let (provider, exporter) = meter();
    provider.force_flush().expect("flush metrics");
    let mut newest = None;
    for metrics in exporter.get_finished_metrics().expect("metrics") {
        for scope in &metrics.scope_metrics {
            for metric in scope.metrics.iter().filter(|m| m.name == name) {
                let gauge = metric
                    .data
                    .as_any()
                    .downcast_ref::<Gauge<u64>>()
                    .expect("a u64 gauge");
                for point in gauge.data_points.iter().filter(|point| {
                    value_of(&point.attributes, "tenant") == tenant
                        && value_of(&point.attributes, "kind") == "state_timeout"
                }) {
                    newest = Some(point.value);
                }
            }
        }
    }
    newest
}

/// The server's timer rows, kept in the sim store beside the journal.
struct TimerRows(SimEventStore);

#[async_trait::async_trait]
impl StateTimerStore for TimerRows {
    async fn upsert_state_timer(&self, timer: &StateTimerRecord) -> Result<(), PersistenceError> {
        self.0.upsert_state_timer(timer);
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .delete_state_timer(tenant, entity_type, entity_id, arm_id);
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        Ok(self.0.list_state_timers(tenant))
    }
}

/// One server process over `store`, serving `tenant`.
fn server(store: &SimEventStore, tenant: &TenantId, name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    registry.register_tenant(
        tenant.clone(),
        csdl,
        CSDL.to_string(),
        &[("Poll", POLL), ("Job", JOB)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state.state_timer_rows = Some(Arc::new(TimerRows(store.clone())));
    state
        .authz
        .reload_tenant_policies(tenant.as_str(), PERMIT_ALL)
        .expect("the tenant policy parses");
    state
}

/// A server process's runtime: single-threaded, on a paused clock. Dropping
/// it stops every task the process started.
fn process_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime builds")
}

/// Let every task that can run, run. The test task stays runnable, so the
/// paused clock never moves.
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// Move the paused clock on by `by`, a step at a time so each look runs at its
/// own time and arms the next.
async fn advance(by: Duration) {
    let step = Duration::from_secs(1);
    let mut gone = Duration::ZERO;
    while gone < by {
        tokio::time::advance(step).await;
        settle().await;
        gone += step;
    }
}

async fn start(state: &ServerState, tenant: &TenantId, entity_type: &str, id: &str) {
    state
        .get_or_create_tenant_entity(tenant, entity_type, id, json!({}))
        .await
        .unwrap_or_else(|error| panic!("{entity_type} create: {error}"));
    let answer = state
        .dispatch_tenant_action(
            tenant,
            entity_type,
            id,
            "Start",
            json!({}),
            &AgentContext::default(),
        )
        .await
        .unwrap_or_else(|error| panic!("{entity_type} Start: {error}"));
    assert!(answer.success, "{entity_type} Start: {answer:?}");
    settle().await;
}

async fn status(state: &ServerState, tenant: &TenantId, entity_type: &str, id: &str) -> String {
    state
        .get_tenant_entity_state(tenant, entity_type, id)
        .await
        .map(|response| response.state.status)
        .unwrap_or_else(|error| format!("<unreadable: {error}>"))
}

/// Sample the gauge and read how many of `tenant`'s state timer rows it counts,
/// and how late it says the oldest is, in seconds.
async fn sample(state: &ServerState, tenant: &TenantId) -> (Option<u64>, Option<u64>) {
    state
        .record_overdue_timer_rows(Duration::from_secs(10))
        .await;
    (
        state_timer_gauge("temper_timer_rows_overdue", tenant.as_str()),
        state_timer_gauge("temper_timer_rows_overdue_oldest_seconds", tenant.as_str()),
    )
}

#[test]
fn dst_a_quiet_poll_is_not_late_and_a_lost_deadline_still_is() {
    meter();
    for seed in 0..SEEDS {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let mut rng = DeterministicRng::new(seed ^ 0x9e11_0a7e);
        let store = SimEventStore::no_faults(seed);
        let tenant = TenantId::new(format!("quiet-{seed}"));
        // Well inside the job's hour.
        let looks = 2 + rng.next_bound(10) as u64;
        // Down past the job's deadline, by up to two hours.
        let downtime = DEADLINE + Duration::from_secs(60 + rng.next_bound(7200) as u64);
        let trace = format!("seed {seed}: {looks} looks, down {downtime:?}");

        process_runtime().block_on(async {
            clock.begin();
            let first = server(&store, &tenant, &format!("quiet-{seed}-a"));
            start(&first, &tenant, "Poll", "poll").await;
            start(&first, &tenant, "Job", "job").await;
            advance(LOOK * looks as u32 + Duration::from_secs(1)).await;

            let row = store
                .list_state_timers(tenant.as_str())
                .into_iter()
                .find(|row| row.entity_type == "Poll")
                .unwrap_or_else(|| panic!("{trace}: the poll keeps its row"));
            let row_due = DateTime::parse_from_rfc3339(row.due_at.as_deref().expect("dated"))
                .expect("a due time");
            assert!(
                row_due < sim_now(),
                "{trace}: quiet looks leave the row at its first due time, past due: {row:?}"
            );
            let (rows, late) = sample(&first, &tenant).await;
            assert_eq!(
                rows,
                Some(0),
                "{trace}: a poll waiting for its next look is not on the gauge"
            );
            assert_eq!(late, Some(0), "{trace}: nothing on the gauge is late");
            clock.end(downtime);
        });

        process_runtime().block_on(async {
            clock.begin();
            let second = server(&store, &tenant, &format!("quiet-{seed}-b"));
            let (rows, _) = sample(&second, &tenant).await;
            assert_eq!(
                rows,
                Some(2),
                "{trace}: before a boot arms them no process holds either timer"
            );

            let walk = second.start_runtime().await.expect("the first start");
            walk.await.expect("the boot walk ends");
            settle().await;
            let counted = rearms_by_bucket(tenant.as_str());
            assert_eq!(
                counted.get("overdue").copied().unwrap_or(0),
                1,
                "{trace}: only the job's deadline was lost: {counted:?}"
            );
            assert_eq!(
                counted.get("repeating").copied().unwrap_or(0),
                1,
                "{trace}: the poll's look is counted as a repeating timer's: {counted:?}"
            );

            advance(LOOK + Duration::from_secs(1)).await;
            assert_eq!(
                status(&second, &tenant, "Job", "job").await,
                "Done",
                "{trace}: the lost deadline fires after the boot"
            );
            assert_eq!(
                status(&second, &tenant, "Poll", "poll").await,
                "Watching",
                "{trace}: the poll goes on"
            );
            let (rows, late) = sample(&second, &tenant).await;
            assert_eq!(rows, Some(0), "{trace}: the gauge is empty again");
            assert_eq!(late, Some(0), "{trace}: nothing on the gauge is late");
            clock.end(Duration::ZERO);
        });
    }
}
