//! A failed boot listing must not turn saved timers into rowless records.
//! Both caller-bound and unbound actions keep their original identity and
//! deadline across seeded listing failures and process restarts.

use super::*;
use temper_authz::{PrincipalKind, SecurityContext};

// These fault controls belong only to this workload; the timer-row store is
// also compiled by the scheduled-action DST, which does not use them.
impl SimTimerRows {
    /// Fail the next `count` boot listings for one tenant.
    fn fail_listings(&self, tenant: &str, count: usize) {
        self.listing_failures
            .lock()
            .unwrap()
            .insert(tenant.to_string(), count);
    }

    /// How many injected listing failures this tenant has yet to encounter.
    fn remaining_listing_failures(&self, tenant: &str) -> usize {
        self.listing_failures
            .lock()
            .unwrap()
            .get(tenant)
            .copied()
            .unwrap_or(0)
    }
}

#[test]
fn dst_boot_listing_retries_keep_saved_callers() {
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
    let mut rng = DeterministicRng::new(seed ^ 0x1157);
    let mut run = Run::new(seed);
    let caller_bound = JOB_V1.replace("initial = \"Idle\"", "initial = \"Idle\"\nstrict_action_params = true").replace(
        "name = \"Expire\"",
        "name = \"Expire\"\nparams = [{ name = \"fired_by\", type = \"string\", source = \"authenticated_subject\" }]",
    );
    let mut jobs: Vec<_> = ["alpha", "beta"]
        .into_iter()
        .map(|tenant| Job {
            tenant: TenantId::new(tenant),
            id: "saved-caller".to_string(),
        })
        .collect();
    if seed & 1 == 0 {
        jobs.reverse();
    }
    let first = Process::start(&world, &caller_bound, &job_untimed(), "listing-first");
    let saved = first.runtime.block_on(async {
        for job in &jobs {
            let caller = AgentContext {
                agent_id: Some(format!("{}-caller", job.tenant)),
                security_ctx: Some(SecurityContext::from_verified_jwt(
                    &format!("{}-caller", job.tenant),
                    PrincipalKind::Agent,
                    Some("test"),
                    None,
                    None,
                    None,
                )),
                ..Default::default()
            };
            first
                .state
                .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
                .await
                .unwrap();
            let moved = first
                .state
                .dispatch_tenant_action(&job.tenant, "Job", &job.id, "Start", json!({}), &caller)
                .await
                .unwrap();
            assert!(moved.success);
        }
        settle().await;
        jobs.iter()
            .map(|job| world.rows.row(job.tenant.as_str(), &job.id).unwrap())
            .collect::<Vec<_>>()
    });
    let downtime = Duration::from_secs(rng.next_bound(1200) as u64);
    first.kill(&world, downtime);
    for job in &jobs {
        let failures = 1 + rng.next_bound(3);
        world.rows.fail_listings(job.tenant.as_str(), failures);
        run.note(format!(
            "{}/{}: {failures} failed boot listings after {}s down",
            job.tenant,
            job.id,
            downtime.as_secs()
        ));
    }
    let second = Process::start(&world, &caller_bound, &job_untimed(), "listing-second");
    let result = second.runtime.block_on(async {
        settle().await;
        // Inspect durable identities before any point read can hydrate actors
        // or reconcile timers, which would mask the boot regression.
        for (job, saved) in jobs.iter().zip(&saved) {
            let row = world.rows.row(job.tenant.as_str(), &job.id).unwrap();
            if row.armed_by != saved.armed_by {
                return Err(run.fail(
                    "saved timer callers survive failed boot listings",
                    format!(
                        "{} replaced {} with {}",
                        job.tenant, saved.armed_by, row.armed_by
                    ),
                ));
            }
            let counts = second
                .booted
                .get(&(job.tenant.clone(), "Job".to_string()))
                .copied()
                .unwrap_or_default();
            if world.rows.remaining_listing_failures(job.tenant.as_str()) != 0
                || counts.rearmed != 1
                || counts.waiting != 0
            {
                return Err(run.fail(
                    "saved timer listings are retried before rowless recovery",
                    format!(
                        "{}: counts {counts:?}, failures remaining {}",
                        job.tenant,
                        world.rows.remaining_listing_failures(job.tenant.as_str())
                    ),
                ));
            }
        }
        // Both timers have the same original deadline, despite time down and
        // retry backoff. No record is read or acted on until after that deadline.
        let deadline = chrono::DateTime::parse_from_rfc3339(&saved[0].started_at).unwrap()
            + chrono::Duration::seconds(3600);
        let remaining = (deadline.with_timezone(&chrono::Utc)
            - temper_runtime::scheduler::sim_now())
        .to_std()
        .unwrap();
        advance(remaining - Duration::from_secs(1)).await;
        for job in &jobs {
            let events = temper_runtime::persistence::EventStore::read_events(
                &world.events,
                &format!("{}:Job:{}", job.tenant, job.id),
                0,
            )
            .await
            .unwrap();
            if events.iter().any(|event| event.event_type == "Expire") {
                return Err(run.fail("saved timers do not fire early", job.tenant.to_string()));
            }
        }
        advance(Duration::from_secs(2)).await;
        for job in &jobs {
            let response = second
                .state
                .get_tenant_entity_state(&job.tenant, "Job", &job.id)
                .await
                .unwrap();
            let fires: Vec<_> = response
                .state
                .events
                .iter()
                .filter(|event| event.action == "Expire")
                .collect();
            if response.state.status != "Done" || fires.len() != 1 {
                return Err(run.fail(
                    "saved timers fire once at their original deadline",
                    format!(
                        "{}: state {}, {} fires",
                        job.tenant,
                        response.state.status,
                        fires.len()
                    ),
                ));
            }
            if job.tenant.as_str() == "alpha" && fires[0].params["fired_by"] != "alpha-caller" {
                return Err(run.fail(
                    "caller-bound timers fire as their saved caller",
                    fires[0].params.to_string(),
                ));
            }
        }
        Ok(())
    });
    second.kill(&world, Duration::ZERO);
    result
}
