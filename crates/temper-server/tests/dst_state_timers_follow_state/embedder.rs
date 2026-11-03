//! DST: a server embedded without `temper serve` gets its timers back.
//!
//! An embedder (temperpaw) builds its own startup from kernel calls: a
//! registry, a storage stack, then the kernel's runtime start. Workload, per
//! seed: a record enters `Waiting`, the process stops, and the next one boots
//! the way an embedder does and leaves the record alone.
//!
//! Invariant: the record leaves `Waiting` on its timer, at its deadline.

use super::*;

/// Boot the way an embedder does: registry, storage, then the kernel's
/// runtime start, without awaiting its walk.
fn embedder_boot(world: &World, name: &str) -> Process {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let state = runtime.block_on(async {
        world.clock.begin();
        let mut registry = SpecRegistry::new();
        let csdl = parse_csdl(CSDL).unwrap();
        registry.register_tenant("beta", csdl, CSDL.to_string(), &[("Job", JOB_V1)]);
        let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
        state.set_storage_stack(StorageStack::from_sim(world.events.clone(), None));
        state.state_timer_rows = Some(world.rows.clone());
        let _walk = state.start_runtime().await;
        state
    });
    Process {
        runtime,
        state,
        booted: BootTimers::new(),
    }
}

#[test]
fn dst_an_embedded_server_rearms_its_timers_at_boot() {
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
    let mut rng = DeterministicRng::new(seed ^ 0xe3b3);
    let mut run = Run::new(seed);
    let job = Job {
        tenant: TenantId::new("beta"),
        id: "embedded".to_string(),
    };
    let first = Process::start(&world, JOB_V1, &job_untimed(), &format!("embed-{seed}-a"));
    first.runtime.block_on(async {
        first
            .state
            .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
            .await
            .map_err(|error| run.fail("records are created", error.to_string()))?;
        if !act(&first.state, &job, "Start").await {
            return Err(run.fail("records start", job.id.clone()));
        }
        settle().await;
        Ok(())
    })?;
    // Down for part of the budget, or past it.
    let downtime = Duration::from_secs(rng.next_bound(7200) as u64);
    run.note(format!("restart after {}s down", downtime.as_secs()));
    first.kill(&world, downtime);

    let second = embedder_boot(&world, &format!("embed-{seed}-b"));
    let result = second.runtime.block_on(async {
        settle().await;
        // Nobody acts on the record: past its 3600 s budget, its timer fires.
        for _ in 0..125 {
            advance(Duration::from_secs(30)).await;
        }
        let status = status(&second.state, &job).await;
        if status == "Done" {
            Ok(())
        } else {
            Err(run.fail(
                "an embedded server's timers fire after a restart",
                format!("{}/{} is {status} past its deadline", job.tenant, job.id),
            ))
        }
    });
    second.kill(&world, Duration::ZERO);
    result
}
