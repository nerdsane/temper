//! The DST's processes: what outlives them, how one boots and stops, the
//! cron callback beside it, and the helpers that drive and read records.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

use super::process_clock::ProcessClock;
use super::workload::{CALLBACK_LAG, CSDL, JOB, PERIOD, TENANT};
use super::world::SimTimerRows;

/// What outlives a process: the journal, the timer rows and the clock.
pub(crate) struct World {
    pub(crate) events: SimEventStore,
    pub(crate) rows: Arc<SimTimerRows>,
    pub(crate) clock: Arc<ProcessClock>,
}

/// One server process.
pub(crate) struct Process {
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) state: ServerState,
}

impl Process {
    /// Boot the way every embedder does: specs, storage, then
    /// `start_runtime`. The cron callback runs beside it.
    pub(crate) fn start(world: &World, name: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let state = runtime.block_on(async {
            world.clock.begin();
            let mut registry = SpecRegistry::new();
            let csdl = parse_csdl(CSDL).unwrap();
            registry.register_tenant(TENANT, csdl, CSDL.to_string(), &[("Job", JOB)]);
            let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
            state.set_storage_stack(StorageStack::from_sim(world.events.clone(), None));
            state.state_timer_rows = Some(world.rows.clone());
            let walk = state.start_runtime().await.expect("the first start");
            walk.await.unwrap();
            let callback = cron_callback(state.clone(), world.events.clone());
            tokio::spawn(callback); // determinism-ok: the cron's integration, inside this process
            state
        });
        Self { runtime, state }
    }

    /// Exit: every task of this process stops.
    pub(crate) fn kill(self, world: &World, downtime: Duration) {
        self.runtime.block_on(async { world.clock.end(downtime) });
        let Self { runtime, state } = self;
        drop(state);
        drop(runtime);
    }
}

/// The cron's callback, as CronJob's integration answers each run: once a
/// record's newest event is a `Run`, plan the next run a period after it.
pub(crate) async fn cron_callback(state: ServerState, events: SimEventStore) {
    let prefix = format!("{TENANT}:Job:");
    loop {
        tokio::time::sleep(Duration::from_secs(CALLBACK_LAG as u64)).await;
        for persistence_id in events.list_all_persistence_ids() {
            let Some(id) = persistence_id.strip_prefix(&prefix) else {
                continue;
            };
            let journal = events.read_events(&persistence_id, 0).await;
            let Some(last) = journal.as_deref().ok().and_then(<[_]>::last) else {
                continue;
            };
            if last.event_type == "Run" {
                let next = last.metadata.timestamp + chrono::Duration::seconds(PERIOD);
                act(
                    &state,
                    id,
                    "Plan",
                    json!({ "next_run_at": next.to_rfc3339() }),
                )
                .await;
            }
        }
    }
}

/// One seed's run: its trace, for the report of a broken invariant.
pub(crate) struct Run {
    pub(crate) seed: u64,
    pub(crate) trace: Vec<String>,
}

impl Run {
    pub(crate) fn fail(&self, invariant: &str, detail: String) -> String {
        format!(
            "seed {}: {invariant}: {detail}\ntrace:\n  {}",
            self.seed,
            self.trace.join("\n  ")
        )
    }
}

/// Let every task that can run, run.
pub(crate) async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// Let `by` pass. The paused clock jumps from one timer to the next, so each
/// fires at its own instant.
pub(crate) async fn pass(by: Duration) {
    tokio::time::sleep(by).await;
    settle().await;
}

/// Let time pass until `at`, waking exactly then.
pub(crate) async fn until(at: DateTime<Utc>) {
    tokio::time::sleep((at - sim_now()).to_std().unwrap_or_default()).await;
}

pub(crate) async fn act(
    state: &ServerState,
    id: &str,
    action: &str,
    params: serde_json::Value,
) -> bool {
    state
        .dispatch_tenant_action(
            &TenantId::new(TENANT),
            "Job",
            id,
            action,
            params,
            &AgentContext::default(),
        )
        .await
        .is_ok_and(|response| response.success)
}

/// When each of the record's events of `action` was written, from its
/// journal.
pub(crate) async fn fired_at(events: &SimEventStore, id: &str, action: &str) -> Vec<DateTime<Utc>> {
    events
        .read_events(&format!("{TENANT}:Job:{id}"), 0)
        .await
        .map(|journal| {
            journal
                .iter()
                .filter(|event| event.event_type == action)
                .map(|event| event.metadata.timestamp)
                .collect()
        })
        .unwrap_or_default()
}
