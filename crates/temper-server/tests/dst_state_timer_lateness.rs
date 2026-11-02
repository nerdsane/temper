//! DST: a state timer is counted overdue only when its deadline passed with
//! no timer waiting for it.
//!
//! `temper_state_timeout_armed_on_hydration_total{elapsed_bucket="overdue"}`
//! is how an operator learns that timers run late. Workload, per seed: in
//! one tenant a record enters `Waiting` and takes `Poke`, an action that does
//! not reset its timer, at a seeded time inside its budget, and the server
//! keeps running past the deadline. In another tenant a record does the same,
//! but the process stops before its deadline and the next one boots, after a
//! seeded downtime, once the deadline has passed.
//!
//! Invariants:
//! - each record leaves `Waiting` on its timer;
//! - a timer that fires at its deadline in a running process is not counted
//!   overdue, though the record moved since the timer was armed: its re-arm
//!   at that deadline is counted on time, once;
//! - a timer whose deadline passed while the server was down is counted
//!   overdue, once.
//!
//! Time is the paused clock of each process's runtime, carried across the
//! restart. Timer rows commit with their events, as Turso's do.
//!
//! Its own test binary, so the global meter provider is set before anything
//! in this process records a metric.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use process_clock::ProcessClock;
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{DeterministicIdGen, DeterministicRng, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::StateTimerStore;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const SEEDS: u64 = 16;

/// `JOB`'s `Waiting` budget.
const BUDGET: Duration = Duration::from_secs(3600);

/// `Poke` keeps the record in `Waiting` without resetting its timer.
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
name = "Poke"
from = ["Waiting"]

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
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Poke" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Expire" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
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

/// One server process over `store`, serving `JOB` to each of `tenants`. Its
/// commits write the timer rows it reads.
fn server(store: &SimEventStore, tenants: &[&TenantId], name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    for tenant in tenants {
        let csdl = parse_csdl(CSDL).expect("CSDL parses");
        registry.register_tenant((*tenant).clone(), csdl, CSDL.to_string(), &[("Job", JOB)]);
    }
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state.state_timer_rows = Some(Arc::new(TimerRows(store.clone())));
    for tenant in tenants {
        state
            .authz
            .reload_tenant_policies(tenant.as_str(), PERMIT_ALL)
            .expect("the tenant policy parses");
    }
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

/// Move the paused clock to `at`, and let what falls due run.
async fn advance_to(at: tokio::time::Instant) {
    tokio::time::advance(at.saturating_duration_since(tokio::time::Instant::now())).await;
    settle().await;
}

async fn act(state: &ServerState, tenant: &TenantId, action: &str) {
    let answer = state
        .dispatch_tenant_action(
            tenant,
            "Job",
            "job",
            action,
            json!({}),
            &AgentContext::default(),
        )
        .await
        .unwrap_or_else(|error| panic!("{tenant}: {action}: {error}"));
    assert!(answer.success, "{tenant}: {action}: {answer:?}");
    settle().await;
}

async fn status(state: &ServerState, tenant: &TenantId) -> String {
    state
        .get_tenant_entity_state(tenant, "Job", "job")
        .await
        .map(|response| response.state.status)
        .unwrap_or_else(|error| format!("<unreadable: {error}>"))
}

/// A record's timer fires at its deadline after a `Poke` that did not reset
/// it, and is not counted overdue; a record whose deadline passed while the
/// server was down is counted overdue once, at boot.
#[test]
fn dst_a_timer_is_overdue_only_when_nothing_waited_for_its_deadline() {
    meter();
    for seed in 0..SEEDS {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let mut rng = DeterministicRng::new(seed ^ 0x1a7e_0e7d);
        let store = SimEventStore::no_faults(seed);
        let steady = TenantId::new(format!("steady-{seed}"));
        let restarted = TenantId::new(format!("restarted-{seed}"));
        // `restarted` starts `lead` after `steady`; both are poked after that,
        // inside `steady`'s budget.
        let lead = Duration::from_secs(120 + rng.next_bound(1680) as u64);
        let poke =
            lead + Duration::from_secs(1 + rng.next_bound(3599 - lead.as_secs() as usize) as u64);
        // Down past `restarted`'s deadline, by up to two hours.
        let downtime = lead + Duration::from_secs(60 + rng.next_bound(7200) as u64);
        let trace = format!("seed {seed}: lead {lead:?}, poke at {poke:?}, down {downtime:?}");

        process_runtime().block_on(async {
            clock.begin();
            let first = server(
                &store,
                &[&steady, &restarted],
                &format!("lateness-{seed}-a"),
            );
            let t0 = tokio::time::Instant::now();
            for tenant in [&steady, &restarted] {
                first
                    .get_or_create_tenant_entity(tenant, "Job", "job", json!({}))
                    .await
                    .unwrap_or_else(|error| panic!("{trace}: {tenant} create: {error}"));
            }
            act(&first, &steady, "Start").await;
            advance_to(t0 + lead).await;
            act(&first, &restarted, "Start").await;
            advance_to(t0 + poke).await;
            act(&first, &steady, "Poke").await;
            act(&first, &restarted, "Poke").await;
            advance_to(t0 + BUDGET + Duration::from_secs(30)).await;
            assert_eq!(
                status(&first, &steady).await,
                "Done",
                "{trace}: the timer fires at its deadline after a Poke"
            );
            assert_eq!(
                status(&first, &restarted).await,
                "Waiting",
                "{trace}: the restarted record's deadline is after the stop"
            );
            clock.end(downtime);
        });
        let steady_counts = rearms_by_bucket(steady.as_str());
        assert_eq!(
            steady_counts.get("overdue").copied().unwrap_or(0),
            0,
            "{trace}: a timer that fired at its deadline in a running process was counted \
             overdue: {steady_counts:?}"
        );
        assert_eq!(
            steady_counts.get("on_time").copied().unwrap_or(0),
            1,
            "{trace}: the re-arm at the deadline after a Poke is counted on time, once: \
             {steady_counts:?}"
        );

        process_runtime().block_on(async {
            clock.begin();
            let second = server(
                &store,
                &[&steady, &restarted],
                &format!("lateness-{seed}-b"),
            );
            let walk = second.start_runtime().await.expect("the first start");
            walk.await.expect("the boot walk ends");
            settle().await;
            advance_to(tokio::time::Instant::now() + Duration::from_secs(60)).await;
            assert_eq!(
                status(&second, &restarted).await,
                "Done",
                "{trace}: a timer that fell due while the server was down fires after boot"
            );
            clock.end(Duration::ZERO);
        });
        let restarted_counts = rearms_by_bucket(restarted.as_str());
        assert_eq!(
            restarted_counts.get("overdue").copied().unwrap_or(0),
            1,
            "{trace}: a timer whose deadline passed while the server was down is counted \
             overdue once: {restarted_counts:?}"
        );
    }
}
