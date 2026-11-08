//! Regressions for journal reads that fail during a timer walk: a passing
//! failure during an install walk, and a record that never reads at boot.

use super::*;

/// A timeout added to a nonresident record must survive a transient journal
/// failure without a dispatch, restart, or pre-existing timer to rescue it.
#[test]
fn install_retries_unreadable_record_without_a_timer() {
    let seed = 96;
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let untimed = job_untimed();
    let job = Job {
        tenant: TenantId::new("gamma"),
        id: "retry-read".to_string(),
    };
    let first = Process::start(&world, JOB_V1, &untimed, "read-failure-a");
    first.runtime.block_on(async {
        first
            .state
            .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
            .await
            .unwrap();
        assert!(act(&first.state, &job, "Start").await);
        settle().await;
        assert!(world.rows.row("gamma", &job.id).is_none());
    });
    first.kill(&world, Duration::ZERO);

    let second = Process::start(&world, JOB_V1, &untimed, "read-failure-b");
    second.runtime.block_on(async {
        // Index before injecting the fault: this must fail the record read,
        // not the independently retried listing of the type.
        second
            .state
            .ensure_type_indexed(&job.tenant, "Job")
            .await
            .unwrap();
        install(&second.state, "gamma", JOB_V1);
        world.events.fail_next_reads("gamma:Job:retry-read", 1);
        let state = second.state.clone();
        let follow = tokio::spawn(async move { state.follow_spec_changes().await });
        settle().await;
        assert!(world.rows.row("gamma", &job.id).is_none());
        assert!(
            follow.is_finished(),
            "an unreadable record must not hold back its type's walk"
        );
        follow.await.unwrap();
        // The walk handed the record to a retry of its own, which reads it
        // once storage recovers and arms its timer.
        advance(Duration::from_secs(1)).await;
        assert_eq!(world.rows.row("gamma", &job.id).unwrap().state, "Waiting");
        // No external dispatch after the failed read: the repaired timer alone
        // must move the record out of Waiting.
        advance(Duration::from_secs(3601)).await;
        assert_eq!(status(&second.state, &job).await, "Done");
    });
    second.kill(&world, Duration::ZERO);
}

/// One record whose journal never reads must not hold back the boot walk. The
/// walk finishes its type, the type's other records keep their timers and are
/// not read again, and the broken record is tried again without its type.
/// Before, the walk walked the whole type again, up to once a minute, for as
/// long as that record stayed unreadable, and never finished.
#[test]
fn a_record_that_never_reads_does_not_hold_back_the_boot_walk() {
    let seed = 97;
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let untimed = job_untimed();
    let tenant = TenantId::new("beta");
    let steady = Job {
        tenant: tenant.clone(),
        id: "steady".to_string(),
    };
    let broken = Job {
        tenant: tenant.clone(),
        id: "broken".to_string(),
    };
    let first = Process::start(&world, JOB_V1, &untimed, "never-reads-a");
    first.runtime.block_on(async {
        for job in [&steady, &broken] {
            first
                .state
                .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
                .await
                .unwrap();
            assert!(act(&first.state, job, "Start").await);
        }
        settle().await;
        for job in [&steady, &broken] {
            assert_eq!(world.rows.row("beta", &job.id).unwrap().state, "Waiting");
        }
    });
    first.kill(&world, Duration::ZERO);

    // Every read of the broken record's journal fails until the fault is
    // lifted below.
    world.events.fail_next_reads("beta:Job:broken", usize::MAX);
    let (second, walk) = Process::boot(&world, JOB_V1, &untimed, "never-reads-b");
    second.runtime.block_on(async {
        settle().await;
        assert!(
            walk.is_finished(),
            "one record that cannot be read held back the boot walk"
        );
        let booted = walk.await.unwrap();
        assert_eq!(
            booted[&(tenant.clone(), "Job".to_string())].rearmed,
            2,
            "both rows are armed again: re-arming reads rows, not journals"
        );
        // The broken record stays unreadable for a while: it alone is tried
        // again, and the record that read is left alone.
        let (steady_reads, broken_reads) = (
            world.events.reads("beta:Job:steady"),
            world.events.reads("beta:Job:broken"),
        );
        for _ in 0..10 {
            advance(Duration::from_secs(30)).await;
        }
        assert_eq!(
            world.events.reads("beta:Job:steady"),
            steady_reads,
            "a record that read was read again while another stayed unreadable"
        );
        assert!(
            world.events.reads("beta:Job:broken") > broken_reads,
            "the unreadable record was not tried again"
        );
        world.events.fail_next_reads("beta:Job:broken", 0);
        advance(Duration::from_secs(60)).await;
        // Nobody acts on the records: their timers alone finish them.
        advance(Duration::from_secs(3601)).await;
        assert_eq!(status(&second.state, &steady).await, "Done");
        assert_eq!(status(&second.state, &broken).await, "Done");
    });
    second.kill(&world, Duration::ZERO);
}
