//! The invariants of the state-timer simulation, checked against the timer
//! rows and the records' states.

use std::collections::BTreeMap;

use super::{Job, Run, SEEDS, TENANTS, World, settle, status, timer_action};
use temper_runtime::persistence::EventStore;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::state::BootTimers;

/// Invariant: the boot arms every record it finds in a timed state, a record
/// with no row too, so it leaves none waiting: no `Job` timeout binds a caller.
pub(super) fn check_boot_counts(booted: &BootTimers, run: &Run) -> Result<(), String> {
    for tenant in TENANTS {
        let waiting = booted
            .get(&(TenantId::new(tenant), "Job".to_string()))
            .map_or(0, |counts| counts.waiting);
        if waiting != 0 {
            return Err(run.fail(
                "the boot arms the records left without a row",
                format!("{tenant}/Job: the boot left {waiting} records waiting"),
            ));
        }
    }
    Ok(())
}

/// Invariant: each record has exactly the timer row its current state
/// declares, and none when nothing armed it ([`Run::unarmed`]) or it was
/// deleted. Rows are captured before any read. At restart the expected state
/// is derived from storage, so the timer-only wait keeps untouched records:
/// the status of the snapshot, then each later event's stored `to_status`,
/// which replay keeps whatever the active spec says the action targets.
pub(super) async fn check_rows_follow_state(
    state: &ServerState,
    world: &World,
    jobs: &[Job],
    run: &Run,
    when: &str,
) -> Result<(), String> {
    settle().await;
    let rows: Vec<_> = jobs
        .iter()
        .map(|job| world.rows.row(job.tenant.as_str(), &job.id))
        .collect();
    for (job, row) in jobs.iter().zip(rows) {
        let current = if run.is_deleted(job) {
            "<deleted>".to_string()
        } else if when == "after the restart" {
            let key = format!("{}:Job:{}", job.tenant, job.id);
            let (seq, mut expected) = match world.events.load_snapshot(&key).await.unwrap() {
                Some((seq, bytes)) => {
                    let snapshot: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    (seq, snapshot["status"].as_str().unwrap().to_string())
                }
                None => (0, "Idle".to_string()),
            };
            for event in world.events.read_events(&key, seq).await.unwrap() {
                if let Some(to) = event.payload["to_status"]
                    .as_str()
                    .filter(|to| !to.is_empty())
                {
                    expected = to.to_string();
                }
            }
            expected
        } else {
            status(state, job).await
        };
        let wanted = timer_action(&current)
            .filter(|_| !run.is_unarmed(job))
            .map(|_| current.as_str());
        if row.as_ref().map(|row| row.state.as_str()) != wanted {
            return Err(run.fail(
                "timer rows follow the current state",
                format!(
                    "{when}: {}/{} is {current} but its timer row is {:?}",
                    job.tenant,
                    job.id,
                    row.map(|row| row.state)
                ),
            ));
        }
    }
    Ok(())
}

/// Invariants at the end: every record reached `Done` on its timers, every
/// timer fired once and in its own state, and no row is left.
pub(super) async fn check_all_done(
    state: &ServerState,
    world: &World,
    jobs: &[Job],
    run: &Run,
) -> Result<(), String> {
    for job in jobs.iter().filter(|job| !run.is_deleted(job)) {
        let response = state
            .get_tenant_entity_state(&job.tenant, "Job", &job.id)
            .await
            .map_err(|error| run.fail("records stay readable", error))?;
        let record = &response.state;
        let fires: Vec<_> = record
            .events
            .iter()
            .filter(|event| event.action == "Expire" || event.action == "Approve")
            .collect();
        for event in &fires {
            if timer_action(&event.from_status) != Some(event.action.as_str()) {
                return Err(run.fail(
                    "timers fire only in their state",
                    format!(
                        "{}/{}: {} fired in {}",
                        job.tenant, job.id, event.action, event.from_status
                    ),
                ));
            }
        }
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
    }
    for job in jobs {
        if let Some(row) = world.rows.row(job.tenant.as_str(), &job.id) {
            return Err(run.fail(
                "no timer outlives its state",
                format!("{}/{} keeps a row for {}", job.tenant, job.id, row.state),
            ));
        }
    }
    Ok(())
}

/// Run `run_seed` over every seed and report each invariant it broke, with the
/// seeds that broke it and the first such seed's report.
pub(super) fn check_seeds(run_seed: fn(u64) -> Result<(), String>) {
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    for seed in 0..SEEDS {
        if let Err(report) = run_seed(seed) {
            let invariant = report.split(": ").nth(1).unwrap_or("unknown").to_string();
            let entry = broken
                .entry(invariant)
                .or_insert_with(|| (Vec::new(), report));
            entry.0.push(seed);
        }
    }
    let summary: Vec<String> = broken
        .iter()
        .map(|(invariant, (seeds, first))| format!("{invariant}: seeds {seeds:?}\nfirst: {first}"))
        .collect();
    assert!(
        broken.is_empty(),
        "invariants broken over {SEEDS} seeds:\n\n{}",
        summary.join("\n\n")
    );
}
