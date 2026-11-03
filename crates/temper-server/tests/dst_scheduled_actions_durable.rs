//! DST: a scheduled action outlives a restart and fires once.
//!
//! Workload, per seed: records arm `schedule(...)` and `schedule_at(...)`
//! actions, once (`Fire`), repeating (`Tick`, which schedules the next one),
//! or as a cron (`Plan` stores the next run time and `schedule_at`s `Run` at
//! it; the cron's callback, played by the test as CronJob's integration does,
//! plans the next run a period after each `Run`). Some records meet a store
//! that fails their next write, or writes it and loses the answer, sometimes
//! failing the row listing the retry after a lost answer reads. The process
//! stops before some are due and after others, sometimes right after a fire,
//! before any removal that is not part of its commit. The next process boots
//! with `start_runtime` and nobody but the cron callback touches the records;
//! on some seeds the store cannot list its rows at that boot, and they are
//! listed again a second later.
//!
//! Invariants:
//! - every armed action fires exactly once per arm, never before it is due,
//!   and at its due time when that falls after the boot or while the first
//!   process ran, or soon after the boot when it fell due while the process
//!   was down; a write that failed or may have landed is retried on the same
//!   row, so it is late by its retries, never lost and never run twice;
//! - a repeating action and a cron keep going after the restart, each fire a
//!   period after the one before;
//! - a fire's event and the removal of its row commit together: no stop
//!   leaves a row behind a written fire;
//! - no row is left for a fired action, a repeating record keeps one, a cron
//!   at most one, and every row is armed exactly once in the process.
//!
//! Each server runs in its own single-threaded, paused-clock Tokio runtime;
//! dropping the runtime is the process exit. Scheduled-action rows are kept
//! with the journal in the simulated store, state timer rows in
//! [`SimTimerRows`]; both outlive a restart.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;
#[path = "dst_state_timers_follow_state/world.rs"]
mod world;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use process_clock::ProcessClock;
use serde_json::json;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_runtime::tenant::TenantId;
use temper_server::request_context::ScheduledFire;
use temper_store_sim::SimEventStore;
use world::SimTimerRows;

#[path = "dst_scheduled_actions_durable/process.rs"]
mod process;
#[path = "dst_scheduled_actions_durable/workload.rs"]
mod workload;

use process::*;
use workload::*;

const SEEDS: u64 = 48;

/// One seed's run. Ok: whether it stopped right after a fire whose event was
/// written, with removals outside the fire's commit held.
fn run_seed(seed: u64) -> Result<usize, String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let mut rng = DeterministicRng::new(seed ^ 0x5c4e_d01e);
    let mut run = Run {
        seed,
        trace: Vec::new(),
    };
    let mut arms: BTreeMap<String, Arm> = BTreeMap::new();

    let first = Process::start(&world, &format!("sched-{seed}-a"));
    let (held_fires, stop) = first.runtime.block_on(async {
        let state = &first.state;
        for i in 0..2 + rng.next_bound(5) {
            let id = format!("job-{i}");
            pass(Duration::from_secs(rng.next_bound(120) as u64)).await;
            state
                .get_or_create_tenant_entity(&TenantId::new(TENANT), "Job", &id, json!({}))
                .await
                .map_err(|error| run.fail("records are created", error.to_string()))?;
            let now = sim_now();
            let in_a_period = now + chrono::Duration::seconds(PERIOD);
            let (action, params, kind, due) = match rng.next_bound(4) {
                0 => ("Start", json!({}), Kind::Once, in_a_period),
                1 => {
                    let due = now + chrono::Duration::seconds(rng.next_bound(7200) as i64);
                    let params = json!({ "due": due.to_rfc3339() });
                    ("StartAt", params, Kind::Once, due)
                }
                2 => ("Activate", json!({}), Kind::Repeat, in_a_period),
                _ => {
                    let params = json!({ "next_run_at": in_a_period.to_rfc3339() });
                    ("Plan", params, Kind::Cron, in_a_period)
                }
            };
            if !act(state, &id, action, params).await {
                return Err(run.fail("records arm their actions", format!("{id} {action}")));
            }
            // The record's next write is its first fire's: the store may
            // fail it, or write it and lose the answer.
            let journal = format!("{TENANT}:Job:{id}");
            let faulted = match rng.next_bound(3) {
                1 => {
                    let failures = 1 + rng.next_bound(3);
                    world.events.fail_next_appends(&journal, failures);
                    run.trace
                        .push(format!("{id}: its next {failures} writes fail"));
                    true
                }
                2 => {
                    world.events.lose_next_append_answers(&journal, 1);
                    run.trace
                        .push(format!("{id}: its next write's answer is lost"));
                    // The retry that follows reads the rows its fire wrote;
                    // sometimes that read fails too.
                    if rng.next_bound(2) == 0 {
                        world.events.fail_next_scheduled_lists(TENANT, 1);
                        run.trace.push("the next row listing fails".to_string());
                    }
                    true
                }
                _ => false,
            };
            let arm = Arm { kind, due, faulted };
            run.trace.push(format!("{id}: {action} at {now}, {arm:?}"));
            arms.insert(id, arm);
        }
        settle().await;
        // Stop at a seeded time. Half the time, stop at a one-shot action's
        // due instant instead: either at once, or right after its fire, with
        // every removal outside the fire's commit held, as when the process
        // stops before one. A row removed only after the fire is left behind
        // and the next boot would fire it again.
        let now = sim_now();
        let dues: Vec<(&String, DateTime<Utc>)> = arms
            .iter()
            .filter(|(_, arm)| arm.kind == Kind::Once && arm.due > now)
            .map(|(id, arm)| (id, arm.due))
            .collect();
        let mut held_fire = false;
        if !dues.is_empty() && rng.next_bound(2) == 0 {
            let (id, due) = dues[rng.next_bound(dues.len())];
            if rng.next_bound(2) == 0 {
                until(due).await;
                run.trace.push(format!("stop at {id}'s due {}", sim_now()));
            } else {
                world.events.hold_scheduled_action_deletes(true);
                until(due).await;
                settle().await;
                held_fire = !fired_at(&world.events, id, "Fire").await.is_empty();
                run.trace.push(format!(
                    "stop at {} after {id}'s Fire (written: {held_fire}), removals held",
                    sim_now()
                ));
            }
        } else {
            until(now + chrono::Duration::seconds(rng.next_bound(3600) as i64)).await;
            let turns = rng.next_bound(200);
            for _ in 0..turns {
                tokio::task::yield_now().await;
            }
            run.trace
                .push(format!("stop at {} after {turns} turns", sim_now()));
        }
        Ok::<_, String>((usize::from(held_fire), sim_now()))
    })?;
    let downtime = Duration::from_secs(rng.next_bound(7200) as u64);
    first.kill(&world, downtime);
    world.events.hold_scheduled_action_deletes(false);
    world.events.fail_next_scheduled_lists(TENANT, 0);
    let left_behind = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let mut left_behind = 0;
            let rows = world.events.list_scheduled_actions(TENANT).await.unwrap();
            for row in rows.iter().filter(|row| row.action == "Fire") {
                left_behind += usize::from(
                    !fired_at(&world.events, &row.entity_id, "Fire")
                        .await
                        .is_empty(),
                );
            }
            left_behind
        });
    if left_behind > 0 {
        return Err(run.fail(
            "a fire's event and its row's removal commit together",
            format!("{left_behind} rows left behind a written fire"),
        ));
    }

    // On some seeds the store cannot list rows when the second process boots,
    // so its rows are armed only when it lists them again.
    let scan_failed = rng.next_bound(4) == 0;
    if scan_failed {
        world.events.fail_next_scheduled_lists(TENANT, 1);
    }
    let second = Process::start(&world, &format!("sched-{seed}-b"));
    let result = second.runtime.block_on(async {
        let state = &second.state;
        let scan_wait = if scan_failed { SCAN_RETRY_WAIT } else { 0 };
        let restart = Restart {
            stop,
            boot: sim_now() + chrono::Duration::seconds(scan_wait),
        };
        run.trace.push(format!(
            "boot at {}, rows listed {}",
            sim_now(),
            restart.boot
        ));
        let last_due = arms
            .values()
            .map(|arm| arm.due)
            .max()
            .unwrap_or(restart.boot)
            .max(restart.boot);
        until(last_due + chrono::Duration::seconds(3 * PERIOD)).await;
        settle().await;
        let end = sim_now();
        let pending = world.events.list_scheduled_actions(TENANT).await.unwrap();
        for (id, arm) in &arms {
            let rows = pending.iter().filter(|row| &row.entity_id == id).count();
            if world.rows.row(TENANT, id).is_some() {
                return Err(run.fail("scheduled actions keep no state timer row", id.clone()));
            }
            let fires = fired_at(&world.events, id, arm.kind.action()).await;
            // Each fire is due when the arm said, then (for a chain) a period
            // after the fire before it.
            let mut due = arm.due;
            for fire in &fires {
                if !restart.on_time(due, *fire, arm.late()) {
                    return Err(run.fail(
                        "a scheduled action fires once, when it is due",
                        format!(
                            "{id} {:?} due {due}, stop {}, boot {}: fired at {fires:?}",
                            arm.kind, restart.stop, restart.boot
                        ),
                    ));
                }
                due = *fire + chrono::Duration::seconds(PERIOD);
            }
            let (fired, rows_ok) = match arm.kind {
                Kind::Once => (fires.len() == 1, rows == 0),
                // A chain is alive: its next fire is not overdue.
                Kind::Repeat => ((end - due).num_seconds() < arm.late(), rows == 1),
                Kind::Cron => ((end - due).num_seconds() < arm.late(), rows <= 1),
            };
            if !fired || fires.is_empty() {
                return Err(run.fail(
                    "a scheduled action fires exactly once per arm, and a chain keeps going",
                    format!(
                        "{id} {:?} due {}, stop {}, boot {}, end {end}: fired at {fires:?}",
                        arm.kind, arm.due, restart.stop, restart.boot
                    ),
                ));
            }
            if !rows_ok {
                return Err(run.fail(
                    "a fired action keeps no row, a chain one",
                    format!("{id} {:?}: {rows} rows", arm.kind),
                ));
            }
        }
        // Every waiting row has exactly one live timer in this process.
        let rows: BTreeMap<ScheduledFire, usize> = pending
            .iter()
            .map(|row| {
                let fire = ScheduledFire {
                    tenant: row.tenant.clone(),
                    entity_type: row.entity_type.clone(),
                    entity_id: row.entity_id.clone(),
                    arm_id: row.arm_id.clone(),
                };
                (fire, 1)
            })
            .collect();
        let armed = state.armed_scheduled_actions();
        if armed != rows {
            return Err(run.fail(
                "every waiting row is armed once in the process",
                format!("rows {rows:?}, armed {armed:?}"),
            ));
        }
        Ok(())
    });
    second.kill(&world, Duration::ZERO);
    result.map(|()| held_fires)
}

#[test]
fn dst_scheduled_actions_outlive_a_restart() {
    // Seeds that broke each invariant, and the first such seed's report.
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    let mut held_fires = 0;
    for seed in 0..SEEDS {
        match run_seed(seed) {
            Ok(fires) => held_fires += fires,
            Err(report) => {
                let invariant = report.split(": ").nth(1).unwrap_or("unknown").to_string();
                let entry = broken
                    .entry(invariant)
                    .or_insert_with(|| (Vec::new(), report));
                entry.0.push(seed);
            }
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
    // A row removed only after its fire is left behind only by a stop right
    // after a fire; a harness that never stops there cannot catch it.
    assert!(
        held_fires > 0,
        "no stop over {SEEDS} seeds came right after a written fire"
    );
}

#[path = "dst_scheduled_actions_durable/fractional.rs"]
mod fractional;
#[path = "dst_scheduled_actions_durable/lapsed.rs"]
mod lapsed;
