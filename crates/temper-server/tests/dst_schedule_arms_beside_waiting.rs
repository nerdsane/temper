//! DST: arms of one action from different commits, and the count that names
//! them.
//!
//! `schedule` writes one durable row per arm, and only an identical due time is
//! dropped. So a try that arms the next try, plus a second way in, runs two
//! chains that never merge. Workload, per seed:
//! - `chains`: `Change` twice, each asking for a `Try`, and each `Try` arms the
//!   next. Two chains, twice the tries.
//! - `one`: `Change` once. One chain: the fire's own row is not beside its
//!   re-arm.
//! - `items`: `AddItem` twice, one `Expire` each. Both are meant to wait.
//! - `reminded`: `Remind` arms `Nudge` twice in one commit.
//! - `aimed`: `Aim` twice at one deadline, which keeps one row.
//!
//! The invariant [`one_commits_arms_waiting`] does not hold today, and this test
//! says so: `chains` and `items` break it. It stays in the test so that making
//! a later arm replace a waiting one fails here until this test is changed on
//! purpose, with `chains` holding the invariant and `items` as the behaviour
//! that changes. A spec that wants that keeps both under two action names.
//!
//! Its own binary: it installs the meter before runtime instruments initialize.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use process_clock::ProcessClock;
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const TENANT: &str = "alpha";
const SEEDS: u64 = 16;

/// `Change`'s and `Try`'s delays in the spec.
const FIRST_TRY: i64 = 60;
const NEXT_TRY: i64 = 3600;

/// How many tries one chain makes in the run.
const TRIES_PER_CHAIN: usize = 4;

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Open"]
initial = "Open"
allow_indefinite_states = ["Open"]

[[state]]
name = "tries"
type = "counter"
initial = 0

[[state]]
name = "expired"
type = "counter"
initial = 0

[[state]]
name = "nudges"
type = "counter"
initial = 0

[[state]]
name = "checks"
type = "counter"
initial = 0

[[action]]
name = "Change"
from = ["Open"]
effect = ["schedule('Try', 60)"]

[[action]]
name = "Try"
from = ["Open"]
effect = ["tries += 1", "schedule('Try', 3600)"]

[[action]]
name = "AddItem"
from = ["Open"]
effect = ["schedule('Expire', 600)"]

[[action]]
name = "Expire"
from = ["Open"]
effect = ["expired += 1"]

[[action]]
name = "Remind"
from = ["Open"]
effect = ["schedule('Nudge', 600)", "schedule('Nudge', 1200)"]

[[action]]
name = "Nudge"
from = ["Open"]
effect = ["nudges += 1"]

[[action]]
name = "Aim"
from = ["Open"]
params = ["check_at"]
effect = ["schedule_at('Check', check_at)"]

[[action]]
name = "Check"
from = ["Open"]
effect = ["checks += 1"]
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="check_at" Type="Edm.String"/>
</EntityType>
<Action Name="Change" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Try" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="AddItem" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Expire" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Remind" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Nudge" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Aim" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/><Parameter Name="check_at" Type="Edm.String"/></Action>
<Action Name="Check" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// Let every task that can run, run.
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// Let time pass until `at`, then let what it woke finish.
async fn until(at: DateTime<Utc>) {
    tokio::time::sleep((at - sim_now()).to_std().unwrap_or_default()).await;
    settle().await;
}

async fn act(state: &ServerState, id: &str, action: &str, params: serde_json::Value) {
    let response = state
        .dispatch_tenant_action(
            &TenantId::new(TENANT),
            "Job",
            id,
            action,
            params,
            &AgentContext::default(),
        )
        .await
        .unwrap_or_else(|error| panic!("{id} {action}: {error}"));
    assert!(response.success, "{id} {action} lands");
    settle().await;
}

/// The arm ids of the record's waiting rows of `action`.
async fn waiting(events: &SimEventStore, id: &str, action: &str) -> Vec<String> {
    events
        .list_scheduled_actions(TENANT)
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.entity_id == id && row.action == action)
        .map(|row| row.arm_id)
        .collect()
}

/// A record's waiting arms of one action all come from one commit. An arm id is
/// `<the arming event's sequence>.<its place in that event>`.
async fn one_commits_arms_waiting(
    events: &SimEventStore,
    id: &str,
    action: &str,
) -> Result<(), String> {
    let arms = waiting(events, id, action).await;
    let commits: BTreeSet<&str> = arms
        .iter()
        .filter_map(|arm| arm.split('.').next())
        .collect();
    if commits.len() > 1 {
        return Err(format!("{id}: {action} waits from commits {commits:?}"));
    }
    Ok(())
}

/// How many events of `action` the record has written.
async fn fired(events: &SimEventStore, id: &str, action: &str) -> usize {
    events
        .read_events(&format!("{TENANT}:Job:{id}"), 0)
        .await
        .unwrap()
        .iter()
        .filter(|event| event.event_type == action)
        .count()
}

/// The counter's total by action, checking its labels and that it reports under
/// the app's id.
fn beside_waiting(
    provider: &SdkMeterProvider,
    exporter: &InMemoryMetricExporter,
) -> BTreeMap<String, u64> {
    provider.force_flush().unwrap();
    let mut counted = BTreeMap::new();
    for batch in exporter.get_finished_metrics().unwrap() {
        for scope in batch.scope_metrics {
            for metric in scope
                .metrics
                .into_iter()
                .filter(|metric| metric.name == "temper_schedule_arms_beside_waiting_total")
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
                    assert_eq!(attrs.len(), 3, "tenant, entity_type and action: {attrs:?}");
                    assert_eq!(attrs["tenant"], TENANT);
                    assert_eq!(attrs["entity_type"], "Job");
                    counted.insert(attrs["action"].clone(), point.value);
                }
            }
        }
    }
    counted
}

/// One seed's run, on its own process.
fn run_seed(seed: u64) {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let mut rng = DeterministicRng::new(seed ^ 0x0a2e_c4a1);
    let events = SimEventStore::no_faults(seed);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        clock.begin();
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            TENANT,
            parse_csdl(CSDL).unwrap(),
            CSDL.to_string(),
            &[("Job", JOB)],
        );
        let mut state = ServerState::from_registry(ActorSystem::new("beside-waiting"), registry);
        state.set_storage_stack(StorageStack::from_sim(events.clone(), None));
        let walk = state.start_runtime().await.expect("the first start");
        walk.await.unwrap();
        let state = &state;
        for id in ["chains", "one", "items", "reminded", "aimed"] {
            state
                .get_or_create_tenant_entity(&TenantId::new(TENANT), "Job", id, json!({}))
                .await
                .unwrap();
        }

        // Short of FIRST_TRY, so the first arm is still waiting at the second.
        let gap = Duration::from_secs(1 + rng.next_bound(30) as u64);
        let check_at = sim_now() + chrono::Duration::seconds(600);
        let aim = json!({ "check_at": check_at.to_rfc3339() });
        act(state, "chains", "Change", json!({})).await;
        act(state, "one", "Change", json!({})).await;
        act(state, "items", "AddItem", json!({})).await;
        act(state, "reminded", "Remind", json!({})).await;
        act(state, "aimed", "Aim", aim.clone()).await;
        tokio::time::sleep(gap).await;
        act(state, "chains", "Change", json!({})).await;
        act(state, "items", "AddItem", json!({})).await;
        act(state, "aimed", "Aim", aim).await;

        assert_eq!(
            waiting(&events, "chains", "Try").await.len(),
            2,
            "seed {seed}"
        );
        assert_eq!(
            waiting(&events, "items", "Expire").await.len(),
            2,
            "seed {seed}"
        );
        assert_eq!(
            waiting(&events, "reminded", "Nudge").await.len(),
            2,
            "seed {seed}"
        );
        assert_eq!(
            waiting(&events, "aimed", "Check").await.len(),
            1,
            "seed {seed}"
        );
        one_commits_arms_waiting(&events, "chains", "Try")
            .await
            .expect_err("two chains break the invariant today");
        one_commits_arms_waiting(&events, "items", "Expire")
            .await
            .expect_err("two items do too, and they are meant to wait");
        one_commits_arms_waiting(&events, "reminded", "Nudge")
            .await
            .expect("arms of one commit stay together");

        // Past the later chain's last try and short of either chain's next.
        let tries = i64::try_from(TRIES_PER_CHAIN).unwrap() - 1;
        let end = sim_now() + chrono::Duration::seconds(FIRST_TRY + NEXT_TRY * tries + 60);
        until(end).await;

        assert_eq!(
            fired(&events, "one", "Try").await,
            TRIES_PER_CHAIN,
            "seed {seed}: one chain tries once a period"
        );
        assert_eq!(
            fired(&events, "chains", "Try").await,
            TRIES_PER_CHAIN * 2,
            "seed {seed}: two chains try twice as often, and neither ever stops"
        );
        assert_eq!(
            waiting(&events, "chains", "Try").await.len(),
            2,
            "seed {seed}"
        );
        assert_eq!(waiting(&events, "one", "Try").await.len(), 1, "seed {seed}");
        one_commits_arms_waiting(&events, "one", "Try")
            .await
            .expect("one chain holds the invariant at every re-arm");
        assert_eq!(fired(&events, "items", "Expire").await, 2, "seed {seed}");
        assert_eq!(fired(&events, "reminded", "Nudge").await, 2, "seed {seed}");
        assert_eq!(fired(&events, "aimed", "Check").await, 1, "seed {seed}");
        clock.end(Duration::ZERO);
    });
}

/// The count names the two chains and the two items, on every re-arm of the
/// chains, and nothing else: not a single chain's re-arm, not arms of one
/// commit, not an arm dropped as identical.
#[test]
fn dst_arms_from_different_commits_are_counted_and_two_chains_never_merge() {
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

    for seed in 0..SEEDS {
        run_seed(seed);
    }

    // Per seed: the second Change, then every try of both chains arms its next
    // beside the other chain's.
    let per_seed = 1 + u64::try_from(TRIES_PER_CHAIN * 2).unwrap();
    assert_eq!(
        beside_waiting(&provider, &exporter),
        BTreeMap::from([
            ("Expire".to_string(), SEEDS),
            ("Try".to_string(), SEEDS * per_seed),
        ]),
    );
}
