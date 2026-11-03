//! Regression: a timer whose fire gets no answer is fired again until the
//! record answers; one whose fire is refused is not.
//!
//! On 2026-10-24 a record's repeating timer stopped for about 20 minutes
//! around an app install: its fire's dispatch failed before the record
//! answered, the timer removed its row, and nothing armed it again until
//! someone dispatched its action by hand. Owed writes (#216, #217) keep a
//! fire the store failed, whose ask timed out or whose actor stopped; these
//! cases are the other ways a fire gets no answer. Admission defers it: the
//! one permit `Expire` admits is held, as by a dispatch still in flight. Or
//! the record cannot be read: its journal fails every read while its actor
//! is not loaded.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::state::{AdmissionOutcome, AdmissionPermit};
use temper_store_sim::SimEventStore;

use super::process_clock::ProcessClock;
use super::world::SimTimerRows;
use super::{JOB_V1, Job, Process, World, act, advance, settle, status};

/// `Waiting`'s budget in `JOB_V1`, and a second more.
const PAST_THE_BUDGET: Duration = Duration::from_secs(3601);

/// `JOB_V1` where `Expire` admits one dispatch at a time and defers any other
/// at once.
fn expire_admits_one() -> String {
    format!(
        "{JOB_V1}\n[admission]\nmax_concurrent_actions = {{ \"Expire\" = 1 }}\nqueue_timeout_seconds = 0\n"
    )
}

/// `JOB_V1` where `Expire` needs `ready`, which nothing sets: its guard refuses
/// every fire of `Waiting`'s timer.
fn expire_refused() -> String {
    let expire = "name = \"Expire\"\nfrom = [\"Waiting\", \"Review\", \"Done\"]\nto = \"Done\"";
    assert!(
        JOB_V1.contains(expire),
        "JOB_V1 declares Expire as expected"
    );
    JOB_V1
        .replacen(expire, &format!("{expire}\nguard = \"ready\""), 1)
        .replacen(
            "[[action]]",
            "[[state]]\nname = \"ready\"\ntype = \"bool\"\ninitial = false\n\n[[action]]",
            1,
        )
}

/// One gamma record, created and started, under a process whose gamma `Job`
/// is `gamma_spec`; the timer row its `Start` wrote has landed. `clock` is the
/// simulation clock the test installed.
fn started(
    seed: u64,
    clock: Arc<ProcessClock>,
    gamma_spec: &str,
    name: &str,
) -> (World, Process, Job) {
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let job = Job {
        tenant: TenantId::new("gamma"),
        id: name.to_string(),
    };
    let process = Process::start(&world, JOB_V1, gamma_spec, name);
    process.runtime.block_on(async {
        process
            .state
            .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
            .await
            .unwrap();
        assert!(act(&process.state, &job, "Start").await);
        settle().await;
    });
    (world, process, job)
}

/// Hold the one permit `tenant`'s `Expire` admits, as a dispatch in flight
/// does.
async fn hold_expire_permit(state: &ServerState, tenant: &TenantId) -> AdmissionPermit {
    let admission = state
        .registry
        .read()
        .unwrap()
        .get_spec(tenant, "Job")
        .and_then(|spec| spec.automaton.admission.clone())
        .expect("the Job spec declares admission");
    match state
        .admission
        .try_acquire_with_caps(tenant, "Job", "Expire", Some(&admission))
        .await
    {
        AdmissionOutcome::Granted(permit) => permit,
        _ => panic!("the one Expire permit is free"),
    }
}

/// Without a dispatch or a restart, the timer whose fire got no answer keeps
/// its row, which a restart would arm, and finishes the record once the
/// fire can land.
#[test]
fn an_unanswered_fire_is_tried_again_until_it_lands() {
    let seed = 97;
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let (world, process, job) = started(seed, clock, &expire_admits_one(), "unanswered");
    process.runtime.block_on(async {
        let state = &process.state;
        let first = world
            .rows
            .row("gamma", &job.id)
            .expect("Start arms Waiting's timer");

        let in_flight = hold_expire_permit(state, &job.tenant).await;
        advance(PAST_THE_BUDGET).await;
        assert_eq!(
            status(state, &job).await,
            "Waiting",
            "the deferred fire did not run"
        );
        let kept = world
            .rows
            .row("gamma", &job.id)
            .expect("a fire that got no answer leaves its record a timer");
        assert_eq!(kept.arm_id, first.arm_id, "the fire is kept with its row");
        assert_eq!(kept.state, "Waiting");

        // Nobody acts on the record: the timer alone finishes it, within one
        // of the owed write's longest waits (an hour).
        drop(in_flight);
        advance(PAST_THE_BUDGET).await;
        assert_eq!(status(state, &job).await, "Done");
        assert!(
            world.rows.row("gamma", &job.id).is_none(),
            "the answered timer ended"
        );
    });
    process.kill(&world, Duration::ZERO);
}

/// A guard's no is an answer: the timer ends and is not tried again, so a
/// record a guard holds is not polled.
#[test]
fn a_refused_fire_is_not_armed_again() {
    let seed = 98;
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let (world, process, job) = started(seed, clock, &expire_refused(), "refused");
    process.runtime.block_on(async {
        let state = &process.state;
        assert!(
            world.rows.row("gamma", &job.id).is_some(),
            "Start arms Waiting's timer"
        );
        advance(PAST_THE_BUDGET).await;
        assert_eq!(status(state, &job).await, "Waiting", "the guard refused");
        assert!(
            world.rows.row("gamma", &job.id).is_none(),
            "a refused fire is not armed again"
        );
    });
    process.kill(&world, Duration::ZERO);
}

/// A process that stops while it keeps an unanswered fire leaves the row,
/// and the next boot fires it: no record read or outside action rescues it.
#[test]
fn an_unanswered_fire_survives_a_restart() {
    for seed in [99, 100, 101] {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let spec = expire_admits_one();
        let (world, process, job) = started(seed, clock, &spec, "unanswered-restart");
        process.runtime.block_on(async {
            let first = world.rows.row("gamma", &job.id).unwrap();
            let _in_flight = hold_expire_permit(&process.state, &job.tenant).await;
            advance(PAST_THE_BUDGET).await;
            assert_eq!(
                world
                    .rows
                    .row("gamma", &job.id)
                    .expect("an unanswered fire keeps its durable row")
                    .arm_id,
                first.arm_id
            );
        });
        process.kill(&world, Duration::ZERO);
        let restarted = Process::start(&world, JOB_V1, &spec, "unanswered-after-restart");
        restarted.runtime.block_on(async {
            advance(Duration::from_secs(1)).await;
            assert_eq!(status(&restarted.state, &job).await, "Done");
            assert!(world.rows.row("gamma", &job.id).is_none());
        });
        restarted.kill(&world, Duration::ZERO);
    }
}

/// After several unanswered tries, the first answer ends the timer, a guard's
/// refusal included, and removes its row.
#[test]
fn a_later_answer_ends_a_kept_fire() {
    for refused in [false, true] {
        let seed = 102;
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let spec = if refused {
            expire_refused()
                + "\n[admission]\nmax_concurrent_actions = { \"Expire\" = 1 }\nqueue_timeout_seconds = 0\n"
        } else {
            expire_admits_one()
        };
        let (world, process, job) = started(seed, clock, &spec, "answered-later");
        process.runtime.block_on(async {
            let in_flight = hold_expire_permit(&process.state, &job.tenant).await;
            advance(PAST_THE_BUDGET).await;
            advance(PAST_THE_BUDGET).await;
            assert!(world.rows.row("gamma", &job.id).is_some());
            drop(in_flight);
            advance(PAST_THE_BUDGET).await;
            assert_eq!(
                status(&process.state, &job).await,
                if refused { "Waiting" } else { "Done" }
            );
            assert!(
                world.rows.row("gamma", &job.id).is_none(),
                "an answer ends the durable timer: refused={refused}"
            );
        });
        process.kill(&world, Duration::ZERO);
    }
}

/// A fire whose record cannot be read got no answer: the timer keeps its row
/// and fires once reads return. Before, the failed read ended the timer.
#[test]
fn a_fire_on_a_record_that_cannot_be_read_is_tried_again() {
    let seed = 109;
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let (world, process, job) = started(seed, clock, JOB_V1, "unreadable");
    let journal = format!("{}:Job:{}", job.tenant, job.id);
    process.runtime.block_on(async {
        let state = &process.state;
        let first = world
            .rows
            .row("gamma", &job.id)
            .expect("Start arms Waiting's timer");
        // Unload the record, so its fire must read the journal to start it.
        advance(Duration::from_secs(301)).await;
        while state.passivate_idle_actors().await > 0 {}
        settle().await;
        assert_eq!(
            state.active_actor_count(),
            0,
            "the record is unloaded (harness)"
        );

        world.events.fail_next_reads(&journal, usize::MAX);
        advance(PAST_THE_BUDGET).await;
        // Let the fire's read fail for good: the actor's starts fail and its
        // ask is answered, as the paused clock moves.
        for _ in 0..20 {
            advance(Duration::from_secs(30)).await;
        }
        assert!(
            status(state, &job).await.starts_with("<unreadable"),
            "the record cannot be read (harness)"
        );
        let kept = world
            .rows
            .row("gamma", &job.id)
            .expect("a fire on an unreadable record keeps its timer");
        assert_eq!(kept.arm_id, first.arm_id);

        world.events.fail_next_reads(&journal, 0);
        advance(PAST_THE_BUDGET).await;
        for _ in 0..4 {
            advance(Duration::from_secs(30)).await;
        }
        assert_eq!(status(state, &job).await, "Done");
        assert!(
            world.rows.row("gamma", &job.id).is_none(),
            "the timer ended once its fire landed"
        );
    });
    process.kill(&world, Duration::ZERO);
}
