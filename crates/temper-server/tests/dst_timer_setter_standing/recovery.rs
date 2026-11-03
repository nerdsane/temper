//! Read failures retry; a committed fire recovers before asking standing again.

use super::*;
use temper_runtime::persistence::EventStore;

fn world(seed: u64, clock: Arc<ProcessClock>) -> World {
    World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
        people: Mutex::new(["setter".to_string()].into_iter().collect()),
        outsiders: Default::default(),
    }
}

fn forget_parent(state: &temper_server::ServerState) {
    // Eviction and passivation force the next authorization to read storage.
    state.entity_state_cache.lock().unwrap().clear();
    if let Some(actor) = state
        .actor_registry
        .write()
        .unwrap()
        .remove("alpha:Job:parent")
    {
        actor.stop().unwrap();
    }
}

const CONTEXT: &str = r#"
[[state]]
name = "parent_id"
type = "string"
initial = ""

[[context_entity]]
name = "parent"
entity_type = "Job"
id_field = "parent_id"
"#;

const CONTEXT_CEDAR: &str = r#"
permit(principal, action in [Action::"Start", Action::"Hold"], resource is Job);
permit(principal, action == Action::"read", resource is Job)
when { resource.ctx_parent_status == "Idle" };
"#;

/// The same context read is used when setting and firing both kinds of timer.
/// A failed read must never be recorded as lost (or absent) standing.
#[test]
fn dst_timer_context_read_failures_retry() {
    for seed in 0..16 {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let world = world(seed, clock);
        let job = format!("{}{CONTEXT}", specs::JOB);
        let process = Process::start_with_job(&world, "context-read", &job);
        process.runtime.block_on(async {
            let tenant = TenantId::new(TENANT);
            let state = &process.state;
            state
                .authz
                .reload_tenant_policies(TENANT, CONTEXT_CEDAR)
                .unwrap();
            state
                .get_or_create_tenant_entity(&tenant, "Job", "parent", json!({}))
                .await
                .unwrap();
            state
                .get_or_create_tenant_entity(
                    &tenant,
                    "Job",
                    "child",
                    json!({"parent_id": "parent"}),
                )
                .await
                .unwrap();
            let caller = Setter::Stays(By::Themselves).caller("setter");
            let (sets, fires) = if seed % 2 == 0 {
                ("Start", "Expire")
            } else {
                ("Hold", "Take")
            };
            // Half of the seeds also fail the read when the timer is set.
            if seed >= 8 {
                forget_parent(state);
                world.events.fail_next_reads("alpha:Job:parent", usize::MAX);
                let failed = state
                    .dispatch_tenant_action(&tenant, "Job", "child", sets, json!({}), &caller)
                    .await;
                assert!(
                    failed.is_err(),
                    "seed {seed}: unread context must fail the setting move: {failed:?}"
                );
                world.events.fail_next_reads("alpha:Job:parent", 0);
            }
            let armed = state
                .dispatch_tenant_action(&tenant, "Job", "child", sets, json!({}), &caller)
                .await
                .unwrap();
            assert!(armed.success, "seed {seed}: timer must be set: {armed:?}");
            forget_parent(state);
            // Keep storage unavailable beyond the actor's own read retries,
            // so the timer must distinguish an unread context from a denial.
            world.events.fail_next_reads("alpha:Job:parent", usize::MAX);
            pass(WAIT + Duration::from_secs(30)).await;
            let journal = world
                .events
                .read_events("alpha:Job:child", 0)
                .await
                .unwrap();
            assert!(
                !journal.iter().any(|e| e.event_type == fires),
                "seed {seed}: the unread timer must wait"
            );
            // Storage recovers without any dispatch or restart rescuing the fire.
            world.events.fail_next_reads("alpha:Job:parent", 0);
            pass(Duration::from_secs(60)).await;
            let journal = world
                .events
                .read_events("alpha:Job:child", 0)
                .await
                .unwrap();
            assert_eq!(
                journal.iter().filter(|e| e.event_type == fires).count(),
                1,
                "seed {seed}: a transient context failure must not cancel {fires}"
            );
            assert!(state.armed_scheduled_actions().is_empty());
            assert!(
                world
                    .events
                    .list_scheduled_actions(TENANT)
                    .await
                    .unwrap()
                    .is_empty()
            );
        });
        process.kill(&world, Duration::ZERO);
    }
}

/// The first fire itself changes the status that allowed its setter to read.
/// Its lost answer must not strand the successor that committed with it.
#[test]
fn dst_committed_timer_recovers_successors_after_standing_changes() {
    for seed in 0..16 {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let world = world(seed, clock);
        let job = format!(
            r#"{}
effect = ["schedule('Finish', 600)"]

[[action]]
name = "Finish"
kind = "internal"
from = ["Taken"]
to = "Done"
"#,
            specs::JOB
        );
        let process = Process::start_with_job(&world, "committed-fire", &job);
        process.runtime.block_on(async {
            let tenant = TenantId::new(TENANT);
            let state = &process.state;
            state.authz.reload_tenant_policies(TENANT, r#"
permit(principal, action == Action::"Hold", resource is Job);
permit(principal, action == Action::"read", resource is Job)
when { resource.Status != "Taken" };
"#).unwrap();
            state.get_or_create_tenant_entity(&tenant, "Job", "child", json!({})).await.unwrap();
            let caller = Setter::Stays(By::Themselves).caller("setter");
            assert!(state.dispatch_tenant_action(&tenant, "Job", "child", "Hold", json!({}), &caller).await.unwrap().success);
            world.events.land_next_appends_then_fail("alpha:Job:child", 1);
            world.events.fail_next_reads("alpha:Job:child", 1);
            pass(WAIT).await;
            let journal = world.events.read_events("alpha:Job:child", 0).await.unwrap();
            assert_eq!(journal.iter().filter(|e| e.event_type == "Take").count(), 1,
                "seed {seed}: the lost answer must follow a committed Take");
            let rows = world.events.list_scheduled_actions(TENANT).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].action, "Finish");
            assert!(!state.armed_scheduled_actions().keys().any(|fire| fire.arm_id == rows[0].arm_id),
                "seed {seed}: the lost answer must leave recovery work to do");
            // Some recoveries also have to retry reading the successor rows.
            world.events.fail_next_reads("alpha:Job:child", (seed % 2) as usize);
            world.events.fail_next_scheduled_lists(TENANT, (seed % 3) as usize);
            pass(Duration::from_secs(60)).await;
            let armed = state.armed_scheduled_actions();
            assert_eq!(armed.len(), 1, "seed {seed}: a committed successor must be armed before asking standing again: {armed:?}");
            assert_eq!(armed.values().copied().sum::<usize>(), 1);
            assert!(armed.keys().any(|fire| fire.arm_id == rows[0].arm_id));
            // Restore standing for the successor's own fire. No restart or
            // external dispatch may be needed for the committed chain to run.
            state.authz.reload_tenant_policies(TENANT, &world.cedar()).unwrap();
            pass(WAIT).await;
            let journal = world.events.read_events("alpha:Job:child", 0).await.unwrap();
            for action in ["Take", "Finish"] {
                assert_eq!(journal.iter().filter(|e| e.event_type == action).count(), 1,
                    "seed {seed}: {action} must commit exactly once");
            }
            assert!(world.events.list_scheduled_actions(TENANT).await.unwrap().is_empty());
            assert!(state.armed_scheduled_actions().is_empty());
        });
        process.kill(&world, Duration::ZERO);
    }
}

/// A Job whose `Take` enters `Taken`, which `Finish`es on a state timeout.
fn job_with_taken_timeout() -> String {
    format!(
        r#"{}

[[state_timeout]]
state = "Taken"
after_seconds = 600
on_timeout = "Finish"

[[action]]
name = "Finish"
kind = "internal"
from = ["Taken"]
to = "Done"
"#,
        specs::JOB
    )
}

/// Like Postgres, this simulation writes state-timer rows after commit. A
/// committed scheduled fire with a lost answer must finish that bookkeeping
/// without another move, restart, or change to the setter's standing.
#[test]
fn dst_committed_scheduled_fire_recovers_state_timeout() {
    for seed in 0..16 {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let world = world(seed, clock);
        let job = job_with_taken_timeout();
        let process = Process::start_with_job(&world, "committed-state-timeout", &job);
        process.runtime.block_on(async {
            let tenant = TenantId::new(TENANT);
            let state = &process.state;
            state.get_or_create_tenant_entity(&tenant, "Job", "child", json!({})).await.unwrap();
            let caller = Setter::Stays(By::Themselves).caller("setter");
            assert!(state.dispatch_tenant_action(&tenant, "Job", "child", "Hold", json!({}), &caller).await.unwrap().success);
            world.events.land_next_appends_then_fail("alpha:Job:child", 1);
            world.events.fail_next_reads("alpha:Job:child", 1);
            pass(WAIT).await;
            let journal = world.events.read_events("alpha:Job:child", 0).await.unwrap();
            assert_eq!(journal.iter().filter(|e| e.event_type == "Take").count(), 1,
                "seed {seed}: the lost answer must follow a committed Take");
            assert!(world.rows.row(TENANT, "child").is_none(),
                "seed {seed}: the lost answer must leave the state-timer row unwritten");
            world.events.fail_next_reads("alpha:Job:child", (seed % 2) as usize);
            pass(Duration::from_secs(60)).await;
            let row = world.rows.row(TENANT, "child").unwrap_or_else(||
                panic!("seed {seed}: recovering a committed fire must write and arm its state timeout"));
            assert_eq!(row.state, "Taken");
            assert_eq!(state.entity_state_cache.lock().unwrap().peek("alpha:Job:child").map(|cached| cached.status.as_str()), Some("Taken"),
                "seed {seed}: recovering the commit must refresh its cached status");
            let armed_by: serde_json::Value = serde_json::from_str(&row.armed_by).unwrap();
            assert_eq!(armed_by["read_when_set"], true,
                "seed {seed}: the timeout must keep its setter's standing");
            pass(WAIT).await;
            let journal = world.events.read_events("alpha:Job:child", 0).await.unwrap();
            for action in ["Take", "Finish"] {
                assert_eq!(journal.iter().filter(|e| e.event_type == action).count(), 1,
                    "seed {seed}: {action} must commit exactly once without a restart or another move");
            }
            assert!(world.rows.row(TENANT, "child").is_none());
            assert!(state.armed_scheduled_actions().is_empty());
        });
        process.kill(&world, Duration::ZERO);
    }
}

/// A move's timers keep the answer taken before the move. Here the move
/// itself takes away its setter's read, and its answer is lost after it
/// commits: the recovery keeps the answer taken before it, and the timeout
/// it set is withheld when it falls due.
#[test]
fn dst_committed_scheduled_fire_keeps_the_answer_taken_before_it() {
    for seed in 0..16 {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let world = world(seed, clock);
        let job = job_with_taken_timeout();
        let process = Process::start_with_job(&world, "committed-answer", &job);
        process.runtime.block_on(async {
            let tenant = TenantId::new(TENANT);
            let state = &process.state;
            state
                .authz
                .reload_tenant_policies(
                    TENANT,
                    r#"
permit(principal, action == Action::"Hold", resource is Job);
permit(principal, action == Action::"read", resource is Job)
when { resource.Status != "Taken" };
"#,
                )
                .unwrap();
            state
                .get_or_create_tenant_entity(&tenant, "Job", "child", json!({}))
                .await
                .unwrap();
            let caller = Setter::Stays(By::Themselves).caller("setter");
            assert!(
                state
                    .dispatch_tenant_action(&tenant, "Job", "child", "Hold", json!({}), &caller)
                    .await
                    .unwrap()
                    .success
            );
            world
                .events
                .land_next_appends_then_fail("alpha:Job:child", 1);
            world.events.fail_next_reads("alpha:Job:child", 1);
            pass(WAIT).await;
            let journal = world
                .events
                .read_events("alpha:Job:child", 0)
                .await
                .unwrap();
            assert_eq!(
                journal.iter().filter(|e| e.event_type == "Take").count(),
                1,
                "seed {seed}: the lost answer must follow a committed Take"
            );
            assert!(
                world.rows.row(TENANT, "child").is_none(),
                "seed {seed}: the lost answer must leave the state-timer row unwritten"
            );
            world
                .events
                .fail_next_reads("alpha:Job:child", (seed % 2) as usize);
            pass(Duration::from_secs(60)).await;
            let row = world.rows.row(TENANT, "child").unwrap_or_else(|| {
                panic!(
                    "seed {seed}: recovering a committed fire must write and arm its state timeout"
                )
            });
            let armed_by: serde_json::Value = serde_json::from_str(&row.armed_by).unwrap();
            assert_eq!(
                armed_by["read_when_set"], true,
                "seed {seed}: the timeout must keep the answer taken before Take, not after it"
            );
            pass(WAIT).await;
            let journal = world
                .events
                .read_events("alpha:Job:child", 0)
                .await
                .unwrap();
            assert_eq!(
                journal.iter().filter(|e| e.event_type == "Take").count(),
                1,
                "seed {seed}: Take must commit exactly once"
            );
            assert_eq!(
                journal.iter().filter(|e| e.event_type == "Finish").count(),
                0,
                "seed {seed}: the setter may not read the record now, so Finish is withheld"
            );
            assert!(state.armed_scheduled_actions().is_empty());
        });
        process.kill(&world, Duration::ZERO);
    }
}
