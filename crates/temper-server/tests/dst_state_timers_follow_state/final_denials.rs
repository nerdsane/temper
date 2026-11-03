//! Final identity denials end the fire; store failures keep its original arm.
//! Seeds vary arm order, append failures, and whether timers cross a restart.
use super::*;
use temper_authz::{PrincipalKind, SecurityContext};

#[test]
fn dst_final_timer_denials_stop_but_store_failures_keep_their_arm() {
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
    let caller_bound = JOB_V1
        .replace("initial = \"Idle\"", "initial = \"Idle\"\nstrict_action_params = true")
        .replace(
            "name = \"Expire\"",
            "name = \"Expire\"\nparams = [{ name = \"fired_by\", type = \"string\", source = \"authenticated_subject\" }]",
        );
    let person = AgentContext {
        security_ctx: Some(SecurityContext::from_verified_jwt(
            "alice",
            PrincipalKind::Agent,
            Some("test"),
            None,
            None,
            None,
        )),
        ..Default::default()
    };
    let mut cases = [
        ("alpha", "no-context", AgentContext::default()),
        ("alpha", "service", AgentContext::for_service("timer-test")),
        ("alpha", "person", person),
        ("beta", "store-fails", AgentContext::default()),
    ];
    let mut rng = DeterministicRng::new(seed ^ 0xde_71ed);
    for i in (1..cases.len()).rev() {
        cases.swap(i, rng.next_bound(i + 1));
    }
    let failures = 1 + rng.next_bound(3);
    let mut run = Run::new(seed);
    run.note(format!(
        "{failures} failed appends; restart {}",
        seed & 1 == 0
    ));
    let mut process = Process::start(&world, &caller_bound, &job_untimed(), "denials-first");
    process.runtime.block_on(async {
        for (tenant, id, caller) in &cases {
            let tenant = TenantId::new(*tenant);
            process
                .state
                .get_or_create_tenant_entity(&tenant, "Job", id, json!({}))
                .await
                .unwrap();
            assert!(
                process
                    .state
                    .dispatch_tenant_action(&tenant, "Job", id, "Start", json!({}), caller)
                    .await
                    .unwrap()
                    .success
            );
        }
        settle().await;
    });
    let original = world.rows.row("beta", "store-fails").unwrap();
    world
        .events
        .fail_next_appends("beta:Job:store-fails", failures);
    if seed & 1 == 0 {
        process.kill(&world, Duration::ZERO);
        process = Process::start(&world, &caller_bound, &job_untimed(), "denials-restarted");
    }
    let result = process.runtime.block_on(async {
        advance(Duration::from_secs(3600)).await;
        // Inspect the row and journal without reading or moving the records:
        // a point read could rehydrate an actor and mask a missing timer.
        for id in ["no-context", "service"] {
            if world.rows.row("alpha", id).is_some() {
                return Err(run.fail("final denials end their timer", id.to_string()));
            }
            let journal = world.events.dump_journal(&format!("alpha:Job:{id}"));
            if journal.iter().any(|event| event.event_type == "Expire") {
                return Err(run.fail("denied fires write nothing", id.to_string()));
            }
        }
        let retained = world.rows.row("beta", "store-fails");
        if !retained.is_some_and(|row| {
            row.arm_id == original.arm_id
                && row.sequence_nr == original.sequence_nr
                && row.state == "Waiting"
                && row.armed_by == original.armed_by
        }) {
            return Err(run.fail(
                "failed writes retain their original timer",
                "store-fails".into(),
            ));
        }
        if world.events.pending_append_failures("beta:Job:store-fails") != failures - 1 {
            return Err(run.fail(
                "the store fault was exercised",
                "expected one failed append".into(),
            ));
        }
        if world
            .events
            .dump_journal("beta:Job:store-fails")
            .iter()
            .any(|event| event.event_type == "Expire")
        {
            return Err(run.fail(
                "the store fault was exercised",
                "fire already committed".into(),
            ));
        }
        let pending: u64 = process
            .state
            .state_timeout_tracker
            .pending_snapshot()
            .iter()
            .map(|(_, count)| count)
            .sum();
        if pending != 1 {
            return Err(run.fail(
                "only the failed write keeps a live timer",
                format!("{pending} tasks"),
            ));
        }
        // Leave every record untouched while the retained fire retries.
        for _ in 0..8 {
            advance(Duration::from_secs(1)).await;
        }
        for (tenant, id, _) in &cases {
            let journal = world.events.dump_journal(&format!("{tenant}:Job:{id}"));
            let fires: Vec<_> = journal
                .iter()
                .filter(|event| event.event_type == "Expire")
                .collect();
            let allowed = *id == "person" || *id == "store-fails";
            if fires.len() != usize::from(allowed) || world.rows.row(tenant, id).is_some() {
                return Err(run.fail(
                    "only allowed fires commit once and leave no timer",
                    format!("{tenant}/{id}: {} fires", fires.len()),
                ));
            }
            if *id == "person" && fires[0].payload["params"]["fired_by"] != "alice" {
                return Err(run.fail(
                    "allowed fires retain their caller",
                    fires[0].payload.to_string(),
                ));
            }
        }
        let pending: u64 = process
            .state
            .state_timeout_tracker
            .pending_snapshot()
            .iter()
            .map(|(_, count)| count)
            .sum();
        if pending != 0 {
            return Err(run.fail("settled fires stop retrying", format!("{pending} tasks")));
        }
        Ok(())
    });
    process.kill(&world, Duration::ZERO);
    result
}
