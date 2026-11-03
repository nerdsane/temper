//! DST: the timers a boot finds due fire a few at a time.
//!
//! Workload, per seed: records in `alpha` start under the first spec and are
//! never passivated, so none has a snapshot; records in `gamma` start untimed.
//! The process stays down longer than any budget, then boots under the second
//! spec with `gamma` timed. `alpha`'s records replay into their stored
//! `Waiting`, whose new budget replaces the timer their rows name. `gamma`'s
//! records have no row, and the boot arms them from when they entered
//! `Waiting`. Every one of those timers is due at boot. A record's first
//! append in the new process (its timer's fire) takes a minute, so fires that
//! run together start at the same instant.
//!
//! Invariants:
//! - at most [`FIRES_AT_ONCE`] timers start firing at one instant;
//! - the boot leaves no record waiting;
//! - every record reaches `Done` on exactly one timer fire, `gamma`'s too.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use temper_runtime::scheduler::{DeterministicIdGen, DeterministicRng, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

use super::process_clock::ProcessClock;
use super::world::SimTimerRows;
use super::{
    JOB_V1, Job, Process, Run, World, act, advance, check_boot_counts, check_seeds, job_untimed,
    job_v2,
};

/// How many due timers the server lets fire at once (`BOOT_FIRES_AT_ONCE`).
const FIRES_AT_ONCE: usize = 8;

/// How long a record's first append in the restarted process takes.
const FIRE_APPEND: Duration = Duration::from_secs(60);

#[test]
fn dst_due_timers_fire_a_few_at_a_time() {
    check_seeds(run_seed);
}

fn run_seed(seed: u64) -> Result<(), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let mut rng = DeterministicRng::new(seed ^ 0xb0b5);
    let mut run = Run::new(seed);
    let mut jobs = Vec::new();
    for tenant in ["alpha", "gamma"] {
        for i in 0..FIRES_AT_ONCE + 1 + rng.next_bound(4) {
            jobs.push(Job {
                tenant: TenantId::new(tenant),
                id: format!("job-{i}"),
            });
        }
    }

    let first = Process::start(&world, JOB_V1, &job_untimed(), &format!("burst-{seed}-a"));
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
        Ok(())
    })?;
    // Longer than either budget: every timer is due when the server is back.
    let downtime = Duration::from_secs(3601 + rng.next_bound(3600) as u64);
    run.note(format!(
        "started {} records; restart after {}s down",
        jobs.len(),
        downtime.as_secs()
    ));
    first.kill(&world, downtime);
    for job in &jobs {
        world
            .events
            .inject_append_delay(&format!("{}:Job:{}", job.tenant, job.id), FIRE_APPEND);
    }

    let second = Process::start(&world, &job_v2(), JOB_V1, &format!("burst-{seed}-b"));
    second.runtime.block_on(async {
        let state = &second.state;
        check_boot_counts(&second.booted, &run)?;
        for _ in 0..4 * jobs.len() / FIRES_AT_ONCE + 4 {
            advance(FIRE_APPEND).await;
        }
        let mut fired_at: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for job in &jobs {
            let record = state
                .get_tenant_entity_state(&job.tenant, "Job", &job.id)
                .await
                .map_err(|error| run.fail("records stay readable", error))?
                .state;
            let fires: Vec<_> = record
                .events
                .iter()
                .filter(|event| event.action == "Expire" || event.action == "Approve")
                .collect();
            if record.status != "Done" || fires.len() != 1 {
                return Err(run.fail(
                    "every record leaves its timed state on its timer",
                    format!(
                        "{}/{} is {} after {} timer fires",
                        job.tenant,
                        job.id,
                        record.status,
                        fires.len()
                    ),
                ));
            }
            fired_at
                .entry(fires[0].timestamp.to_rfc3339())
                .or_default()
                .push(format!("{}/{}", job.tenant, job.id));
        }
        match fired_at
            .iter()
            .find(|(_, fired)| fired.len() > FIRES_AT_ONCE)
        {
            Some((at, fired)) => Err(run.fail(
                "due timers fire a few at a time",
                format!("{} timers fired at {at}: {fired:?}", fired.len()),
            )),
            None => Ok(()),
        }
    })?;
    second.kill(&world, Duration::ZERO);
    Ok(())
}
