//! DST: a record's armed timer follows its current state.
//!
//! Workload, per seed: records in three tenants enter `Waiting`, timed in
//! `alpha` and `beta` and untimed in `gamma`; some are poked, deleted,
//! passivated and rehydrated. An install then changes two specs: `alpha`'s,
//! whose `Start` now goes to `Review` and whose `Waiting` timer has another
//! budget, and `gamma`'s, which gains the timeouts. Records that already
//! started keep their stored `Waiting` on replay, so their timers must follow
//! `Waiting`'s new declaration and never `Review`'s. In some seeds the process
//! stops before the install's walk reaches `gamma`. The server restarts, so
//! records that were never snapshotted replay under the new specs, and
//! `gamma`'s records may have no row at all.
//!
//! Invariants:
//! - the boot arms every record it finds in a timed state with no row, and
//!   leaves none waiting;
//! - after a record rehydrates, it has exactly one armed timer row, for its
//!   current state, when that state declares a timeout, and none otherwise
//!   (none for a deleted record);
//! - timers fire only in their state: every `Expire` left `Waiting` and every
//!   `Approve` left `Review`, once;
//! - with nobody touching the records after the restart, every record still
//!   reaches `Done` on its timers alone.
//!
//! Each server runs in its own single-threaded, paused-clock Tokio runtime;
//! dropping the runtime is the process exit. The simulation clock reads that
//! paused clock, so `sim_now()` and timer sleeps share one virtual time. The
//! event journal is the simulated store; timer rows live in [`SimTimerRows`],
//! both shared across restarts. Each row write lands after a seeded delay, so
//! the writes of overlapping arms and cancellations land in any order.
//!
//! Two more workloads share this world: `boot_burst` (the timers a boot finds
//! due fire a few at a time) and `delete_race` (a delete while timer rows are
//! still landing leaves no row). So do two regressions: `read_failure` (an
//! install walk retries a record it could not read) and `unanswered_fire` (a
//! timer whose fire got no answer is tried again until it lands; a refused
//! one is not).

#[path = "dst_state_timers_follow_state/boot_burst.rs"]
mod boot_burst;
#[path = "dst_state_timers_follow_state/boot_listing_retry.rs"]
mod boot_listing_retry;
#[path = "dst_state_timers_follow_state/boot_logging.rs"]
mod boot_logging;
#[path = "dst_state_timers_follow_state/delete_race.rs"]
mod delete_race;
#[path = "dst_state_timers_follow_state/embedder.rs"]
mod embedder;
#[path = "dst_state_timers_follow_state/final_denials.rs"]
mod final_denials;
#[path = "dst_state_timers_follow_state/invariants.rs"]
mod invariants;
#[path = "dst_state_timers_follow_state/late_reconcile.rs"]
mod late_reconcile;
#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;
#[path = "dst_state_timers_follow_state/slow_store.rs"]
mod slow_store;
#[path = "dst_state_timers_follow_state/slow_store_env.rs"]
mod slow_store_env;
#[path = "dst_state_timers_follow_state/specs.rs"]
mod specs;
#[path = "dst_state_timers_follow_state/world.rs"]
mod world;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use invariants::{check_all_done, check_boot_counts, check_rows_follow_state, check_seeds};
use process_clock::ProcessClock;
use serde_json::json;
use specs::{CSDL, JOB_V1, install, job_untimed, job_v2, timer_action};
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{DeterministicIdGen, DeterministicRng, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::BootTimers;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use world::SimTimerRows;

const SEEDS: u64 = 48;

/// The tenants of the workload: `gamma` starts with [`job_untimed`].
const TENANTS: [&str; 3] = ["alpha", "beta", "gamma"];

/// What outlives a process: the journal, the timer rows and the clock.
struct World {
    events: SimEventStore,
    rows: Arc<SimTimerRows>,
    clock: Arc<ProcessClock>,
}

/// One server process.
struct Process {
    runtime: tokio::runtime::Runtime,
    state: ServerState,
    /// What its boot did with each timed type's timers, once its walk ended.
    booted: BootTimers,
}

impl Process {
    /// Boot the way every embedder does: specs, storage, then
    /// `start_runtime`, which re-arms the timers the last process left
    /// waiting and walks the timed types' records (awaited here, in the
    /// background in `temper serve`).
    fn start(world: &World, alpha_spec: &str, gamma_spec: &str, name: &str) -> Self {
        let (mut process, walk) = Self::boot(world, alpha_spec, gamma_spec, name);
        process.booted = process.runtime.block_on(walk).unwrap();
        process
    }

    /// Boot as [`Self::start`] does, but return with the walk still running,
    /// as `temper serve` takes requests while it runs.
    fn boot(
        world: &World,
        alpha_spec: &str,
        gamma_spec: &str,
        name: &str,
    ) -> (Self, tokio::task::JoinHandle<BootTimers>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let (state, walk) = runtime.block_on(async {
            world.clock.begin();
            let mut registry = SpecRegistry::new();
            // Restore the persisted specs before reinstalling the current app
            // bundles, as serve does. This must queue real spec changes.
            for (tenant, spec) in [("alpha", JOB_V1), ("gamma", job_untimed().as_str())] {
                registry.register_tenant(
                    tenant,
                    parse_csdl(CSDL).unwrap(),
                    CSDL.to_string(),
                    &[("Job", spec)],
                );
            }
            for (tenant, spec) in [
                ("alpha", alpha_spec),
                ("beta", JOB_V1),
                ("gamma", gamma_spec),
            ] {
                let csdl = parse_csdl(CSDL).unwrap();
                registry.register_tenant(tenant, csdl, CSDL.to_string(), &[("Job", spec)]);
            }
            let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
            state.set_storage_stack(StorageStack::from_sim(world.events.clone(), None));
            state.state_timer_rows = Some(world.rows.clone());
            // Serve reinstalls apps before runtime recovery. No install walk
            // may touch the saved rows or fire an overdue timer here.
            let saved_rows = world.rows.row("alpha", "job-0");
            state.follow_spec_changes().await;
            assert_eq!(
                world
                    .rows
                    .row("alpha", "job-0")
                    .map(|row| (row.state, row.armed_by)),
                saved_rows.map(|row| (row.state, row.armed_by)),
                "startup installs must leave saved timer rows untouched",
            );
            let walk = state.start_runtime().await.expect("the first start");
            (state, walk)
        });
        let booted = BootTimers::new();
        (
            Self {
                runtime,
                state,
                booted,
            },
            walk,
        )
    }

    /// Exit: every task of this process stops.
    fn kill(self, world: &World, downtime: Duration) {
        self.runtime.block_on(async { world.clock.end(downtime) });
        let Self { runtime, state, .. } = self;
        drop(state);
        drop(runtime);
    }
}

/// A record of the workload.
#[derive(Clone)]
struct Job {
    tenant: TenantId,
    id: String,
}

/// One seed's run: its trace and the first invariant it broke.
struct Run {
    seed: u64,
    trace: Vec<String>,
    deleted: BTreeSet<(String, String)>,
    /// Records in a timed state that nothing armed: no dispatch since their
    /// type gained its timeouts, and no install walk. They have no row until
    /// their next action or the next boot arms them.
    unarmed: BTreeSet<(String, String)>,
}

impl Run {
    fn new(seed: u64) -> Self {
        Self {
            seed,
            trace: Vec::new(),
            deleted: BTreeSet::new(),
            unarmed: BTreeSet::new(),
        }
    }

    fn is_unarmed(&self, job: &Job) -> bool {
        self.unarmed
            .contains(&(job.tenant.to_string(), job.id.clone()))
    }

    fn note(&mut self, line: String) {
        self.trace.push(line);
    }

    fn fail(&self, invariant: &str, detail: String) -> String {
        format!(
            "seed {}: {invariant}: {detail}\ntrace:\n  {}",
            self.seed,
            self.trace.join("\n  ")
        )
    }

    fn is_deleted(&self, job: &Job) -> bool {
        self.deleted
            .contains(&(job.tenant.to_string(), job.id.clone()))
    }
}

/// Let every task that can run, run: due timers fire, spawned reconciles
/// finish. The test task stays runnable, so the paused clock never moves.
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

async fn advance(by: Duration) {
    tokio::time::advance(by).await;
    settle().await;
}

async fn act(state: &ServerState, job: &Job, action: &str) -> bool {
    state
        .dispatch_tenant_action(
            &job.tenant,
            "Job",
            &job.id,
            action,
            json!({}),
            &AgentContext::default(),
        )
        .await
        .is_ok_and(|response| response.success)
}

async fn status(state: &ServerState, job: &Job) -> String {
    state
        .get_tenant_entity_state(&job.tenant, "Job", &job.id)
        .await
        .map(|response| response.state.status)
        .unwrap_or_else(|error| format!("<unreadable: {error}>"))
}

/// Passivate a random part of the records: the rest are read first, so only
/// the untouched ones are idle long enough.
async fn passivate_some(
    state: &ServerState,
    jobs: &[Job],
    rng: &mut DeterministicRng,
    run: &mut Run,
) {
    advance(Duration::from_secs(301)).await;
    let mut kept = Vec::new();
    for job in jobs.iter().filter(|job| !run.is_deleted(job)) {
        if rng.chance(0.5) {
            status(state, job).await;
            kept.push(job.id.clone());
        }
    }
    while state.passivate_idle_actors().await > 0 {}
    settle().await;
    run.note(format!("passivated all but {kept:?}"));
}

fn run_seed(seed: u64) -> Result<(), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let mut rng = DeterministicRng::new(seed);
    let mut run = Run::new(seed);
    let mut jobs = Vec::new();
    for tenant in TENANTS {
        for i in 0..3 + rng.next_bound(3) {
            jobs.push(Job {
                tenant: TenantId::new(tenant),
                id: format!("job-{i}"),
            });
        }
    }
    let v2 = job_v2();
    let untimed = job_untimed();

    let first = Process::start(&world, JOB_V1, &untimed, &format!("follow-{seed}-a"));
    first.runtime.block_on(async {
        let state = &first.state;
        for job in &jobs {
            advance(Duration::from_secs(rng.next_bound(60) as u64)).await;
            state
                .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
                .await
                .map_err(|error| run.fail("records are created", error.to_string()))?;
            if !act(state, job, "Start").await {
                return Err(run.fail("records start", format!("{}/{}", job.tenant, job.id)));
            }
        }
        run.note(format!("started {} records", jobs.len()));
        for job in &jobs {
            if rng.chance(0.3) {
                act(state, job, "Poke").await;
                run.note(format!("poked {}/{}", job.tenant, job.id));
            }
        }
        for tenant in TENANTS {
            if rng.chance(0.3) {
                let of_tenant: Vec<_> = jobs
                    .iter()
                    .filter(|job| job.tenant.as_str() == tenant)
                    .collect();
                let job = of_tenant[rng.next_bound(of_tenant.len())];
                state
                    .delete_tenant_entity(&job.tenant, "Job", &job.id)
                    .await
                    .map_err(|error| run.fail("records are deleted", error))?;
                run.deleted.insert((tenant.to_string(), job.id.clone()));
                run.note(format!("deleted {tenant}/{}", job.id));
            }
        }
        settle().await;
        if rng.chance(0.7) {
            passivate_some(state, &jobs, &mut rng, &mut run).await;
        }

        install(state, "alpha", &v2);
        let gamma_followed = rng.chance(0.5);
        if gamma_followed {
            install(state, "gamma", JOB_V1);
            state.follow_spec_changes().await;
        } else {
            state.follow_spec_changes().await;
            // The process stops before the install's walk reaches gamma.
            install(state, "gamma", JOB_V1);
            state.registry.write().unwrap().take_changed_types();
            // Nothing armed gamma's records: they have no row until their next
            // action or the next boot.
            for job in &jobs {
                if job.tenant.as_str() == "gamma" && !run.is_deleted(job) {
                    run.unarmed.insert((job.tenant.to_string(), job.id.clone()));
                }
            }
        }
        settle().await;
        run.note(format!(
            "installed: alpha's Start now enters Review, gamma gains timeouts{}",
            if gamma_followed {
                ""
            } else {
                " (its walk never ran)"
            }
        ));

        let live: Vec<Job> = jobs
            .iter()
            .filter(|job| !run.is_deleted(job))
            .cloned()
            .collect();
        for job in &live {
            match rng.next_bound(3) {
                0 => {}
                1 => {
                    // A dispatch arms the timer its record's state declares.
                    if act(state, job, "Poke").await {
                        run.unarmed
                            .remove(&(job.tenant.to_string(), job.id.clone()));
                    }
                    run.note(format!("poked {}/{}", job.tenant, job.id));
                }
                _ => {
                    status(state, job).await;
                    run.note(format!("read {}/{}", job.tenant, job.id));
                }
            }
        }
        if rng.chance(0.7) {
            passivate_some(state, &jobs, &mut rng, &mut run).await;
        }
        check_rows_follow_state(state, &world, &jobs, &run, "after the install").await
    })?;

    let downtime = Duration::from_secs(rng.next_bound(600) as u64);
    run.note(format!("restart after {}s down", downtime.as_secs()));
    first.kill(&world, downtime);

    let second = Process::start(&world, &v2, JOB_V1, &format!("follow-{seed}-b"));
    // The boot arms the records the install's walk never reached.
    run.unarmed.clear();
    second.runtime.block_on(async {
        let state = &second.state;
        check_boot_counts(&second.booted, &run)?;
        check_rows_follow_state(state, &world, &jobs, &run, "after the restart").await?;
        // A read rehydrates a record, which follows its state.
        let looked: Vec<Job> = jobs.iter().filter(|_| rng.chance(0.4)).cloned().collect();
        run.note(format!(
            "rehydrated {:?}",
            looked
                .iter()
                .map(|job| format!("{}/{}", job.tenant, job.id))
                .collect::<Vec<_>>()
        ));
        for job in &looked {
            status(state, job).await;
        }
        settle().await;
        // Nobody acts on the records: their timers alone must finish them.
        for _ in 0..360 {
            advance(Duration::from_secs(30)).await;
        }
        check_all_done(state, &world, &jobs, &run).await
    })?;
    second.kill(&world, Duration::ZERO);
    Ok(())
}

#[path = "dst_state_timers_follow_state/read_failure.rs"]
mod read_failure;
#[path = "dst_state_timers_follow_state/unanswered_fire.rs"]
mod unanswered_fire;

#[test]
fn dst_state_timers_follow_the_current_state() {
    check_seeds(run_seed);
}
