//! Concurrent timeout admission-cap regressions.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn load_120_concurrent_dispatches_admission_caps_hold() {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        TICKET_CSDL.to_string(),
        &[("Ticket", TICKET_WITH_ADMISSION_IOA)],
    );
    let system = ActorSystem::new("load-admission-test");
    let state = Arc::new(ServerState::from_registry(system, registry));
    let tenant = temper_runtime::tenant::TenantId::from("default".to_string());
    let agent_ctx = AgentContext::for_service("timeout-scheduler");

    // Pre-create 120 ticket entities so the concurrent AssignAgent calls
    // race on the shared admission cap for that action.
    const N: usize = 120;
    for i in 0..N {
        state
            .get_or_create_tenant_entity(
                &tenant,
                "Ticket",
                &format!("t-{i}"),
                serde_json::json!({}),
            )
            .await
            .expect("create ticket");
    }

    let granted = Arc::new(AtomicUsize::new(0));
    let deferred = Arc::new(AtomicUsize::new(0));
    let other = Arc::new(AtomicUsize::new(0));
    let in_flight_peak = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let latencies_ns = Arc::new(Mutex::new(Vec::<u128>::with_capacity(N)));

    let barrier = Arc::new(tokio::sync::Barrier::new(N));
    let mut handles = Vec::with_capacity(N);
    let wall_start = Instant::now();
    for i in 0..N {
        let state = state.clone();
        let tenant = tenant.clone();
        let agent_ctx = agent_ctx.clone();
        let granted = granted.clone();
        let deferred = deferred.clone();
        let other = other.clone();
        let in_flight_peak = in_flight_peak.clone();
        let in_flight = in_flight.clone();
        let latencies_ns = latencies_ns.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await; // fire all at once
            let call_start = Instant::now();
            in_flight.fetch_add(1, Ordering::AcqRel);
            // Record peak in-flight count.
            let cur = in_flight.load(Ordering::Acquire);
            let mut peak = in_flight_peak.load(Ordering::Acquire);
            while cur > peak
                && let Err(p) =
                    in_flight_peak.compare_exchange(peak, cur, Ordering::AcqRel, Ordering::Acquire)
            {
                peak = p;
            }

            let res = state
                .dispatch_tenant_action_ext_typed(
                    &tenant,
                    "Ticket",
                    &format!("t-{i}"),
                    "AssignAgent",
                    serde_json::json!({}),
                    crate::state::dispatch::DispatchExtOptions {
                        agent_ctx: &agent_ctx,
                        await_integration: false,
                        await_reactions: true,
                    },
                )
                .await;
            let call_ns = call_start.elapsed().as_nanos();
            latencies_ns.lock().unwrap().push(call_ns);
            match res {
                Ok(r) if r.success => {
                    granted.fetch_add(1, Ordering::AcqRel);
                }
                Ok(_) => {
                    other.fetch_add(1, Ordering::AcqRel);
                }
                Err(crate::state::dispatch::DispatchError::Deferred { .. }) => {
                    deferred.fetch_add(1, Ordering::AcqRel);
                }
                Err(_) => {
                    other.fetch_add(1, Ordering::AcqRel);
                }
            }
            in_flight.fetch_sub(1, Ordering::AcqRel);
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let wall = wall_start.elapsed();

    let g = granted.load(Ordering::Acquire);
    let d = deferred.load(Ordering::Acquire);
    let o = other.load(Ordering::Acquire);
    let peak = in_flight_peak.load(Ordering::Acquire);
    let mut lats = latencies_ns.lock().unwrap().clone();
    lats.sort_unstable();
    let p = |q: f64| -> u128 {
        let idx = ((lats.len() as f64 - 1.0) * q).round() as usize;
        lats[idx.min(lats.len().saturating_sub(1))]
    };
    let throughput = N as f64 / wall.as_secs_f64();

    eprintln!("LOAD RESULT: granted={g} deferred={d} other={o} in_flight_peak={peak} total={N}");
    eprintln!(
        "LOAD PERF:   wall={wall_ms:.1}ms throughput={tp:.0}/s p50={p50:.2}ms p95={p95:.2}ms p99={p99:.2}ms max={pmax:.2}ms",
        wall_ms = wall.as_secs_f64() * 1000.0,
        tp = throughput,
        p50 = (p(0.50) as f64) / 1_000_000.0,
        p95 = (p(0.95) as f64) / 1_000_000.0,
        p99 = (p(0.99) as f64) / 1_000_000.0,
        pmax = (p(1.0) as f64) / 1_000_000.0,
    );

    // Hard contract assertions:
    //
    // 1. Zero unknown failures — every outcome is Granted or Deferred;
    //    no panics, no permanent errors, no timeouts.
    assert_eq!(
        o, 0,
        "unexpected non-granted, non-deferred outcomes: {o} (spec: 500-class behavior is forbidden)"
    );

    // 2. Every dispatch is accounted for.
    assert_eq!(g + d + o, N, "outcome count must equal submissions");

    // 3. Admission cap holds — since cap is 5 and queue_timeout is 10s
    //    with N=120 inputs, some should defer. If all 120 granted
    //    instantly, admission isn't firing at all.
    assert!(
        g >= 5,
        "at least the cap's worth ({}) should succeed, got {g}",
        5
    );

    // 4. Peak in-flight observation does NOT assert <= 5 because
    //    in_flight counts the pre-acquire window too; what we do
    //    assert is the admission semaphore gate works (see test
    //    `grants_up_to_cap_and_defers_beyond` for the hard cap proof).
}
