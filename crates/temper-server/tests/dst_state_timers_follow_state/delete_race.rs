//! DST: a deleted record keeps no timer row, whatever row writes were still
//! landing when it was deleted.
//!
//! Workload, per seed: records in `alpha` start under a spec where `Poke`
//! resets `Waiting`'s clock, and the process stops. It boots under the spec
//! that gives `Waiting` another budget, and takes requests while the boot walk
//! runs: after seeded waits each record is poked, then deleted. So a reconcile
//! (the boot walk's or the record's rehydration) replaces the row's timer with
//! `Waiting`'s new one, the `Poke` arms a fresh one over it while the first
//! one's row is being written, and the delete cancels the fresh one while
//! rows may still be landing. Row
//! writes take up to 47 scheduler turns, so they overlap all of this.
//!
//! Invariant: once every write has landed, no deleted record has a row.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use temper_runtime::scheduler::{DeterministicIdGen, DeterministicRng, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

use super::process_clock::ProcessClock;
use super::world::SimTimerRows;
use super::{
    JOB_V1, Job, Process, Run, World, act, advance, check_seeds, job_untimed, job_v2, settle,
};

#[test]
fn dst_a_deleted_record_keeps_no_timer_row() {
    check_seeds(run_seed);
}

/// `spec` with `Poke` resetting `Waiting`'s clock.
fn poke_resets_waiting(spec: &str) -> String {
    spec.replacen(
        "on_timeout = \"Expire\"",
        "on_timeout = \"Expire\"\nreset_on = [\"Poke\"]",
        1,
    )
}

/// Give the other tasks `turns` scheduler turns.
async fn turns(turns: usize) {
    for _ in 0..turns {
        tokio::task::yield_now().await;
    }
}

fn run_seed(seed: u64) -> Result<(), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::with_round_trips(seed, 48)),
        clock,
    };
    let mut rng = DeterministicRng::new(seed ^ 0xde1e7e);
    let mut run = Run::new(seed);
    let jobs: Vec<Job> = (0..4 + rng.next_bound(5))
        .map(|i| Job {
            tenant: TenantId::new("alpha"),
            id: format!("job-{i}"),
        })
        .collect();
    let untimed = job_untimed();

    let v1 = poke_resets_waiting(JOB_V1);
    let first = Process::start(&world, &v1, &untimed, &format!("race-{seed}-a"));
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
    first.kill(&world, Duration::from_secs(rng.next_bound(60) as u64));

    let v2 = poke_resets_waiting(&job_v2());
    let (second, walk) = Process::boot(&world, &v2, &untimed, &format!("race-{seed}-b"));
    second.runtime.block_on(async {
        let mut requests = Vec::new();
        for job in &jobs {
            let (state, job) = (second.state.clone(), job.clone());
            let (before_poke, before_delete) = (rng.next_bound(64), rng.next_bound(64));
            run.note(format!(
                "{}/{}: poke after {before_poke} turns, delete {before_delete} turns later",
                job.tenant, job.id
            ));
            // determinism-ok: a request on the single-threaded test runtime.
            requests.push(tokio::spawn(async move {
                turns(before_poke).await;
                act(&state, &job, "Poke").await;
                turns(before_delete).await;
                state
                    .delete_tenant_entity(&job.tenant, "Job", &job.id)
                    .await
            }));
        }
        for (job, request) in jobs.iter().zip(requests) {
            request
                .await
                .unwrap()
                .map_err(|error| run.fail("records are deleted", error))?;
            run.deleted.insert((job.tenant.to_string(), job.id.clone()));
        }
        walk.await.unwrap();
        settle().await;
        for job in &jobs {
            if let Some(row) = world.rows.row(job.tenant.as_str(), &job.id) {
                return Err(run.fail(
                    "a deleted record keeps no timer row",
                    format!("{}/{} keeps a row for {}", job.tenant, job.id, row.state),
                ));
            }
        }
        Ok(())
    })?;
    second.kill(&world, Duration::ZERO);
    Ok(())
}
