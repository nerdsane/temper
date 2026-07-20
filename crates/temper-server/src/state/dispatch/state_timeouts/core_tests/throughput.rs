//! Focused state-timeout regression group.

use super::*;

/// 1000-way sustained-throughput measurement: cap=50 (realistic
/// production-style cap), each call hits a unique entity so work
/// parallelizes across actors. Measures steady-state dispatch cost
/// through the full retry + admission + dispatch path.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn load_1000_throughput_baseline() {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    const THROUGHPUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]
initial = "Open"
allow_indefinite_states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]

[[state]]
name = "replies"
type = "counter"
initial = "0"

[[action]]
name = "AssignAgent"
kind = "input"
from = ["Open"]
to = "InProgress"

[admission]
max_concurrent_creates = 50
max_concurrent_actions = { "AssignAgent" = 50 }
queue_depth = 2000
queue_timeout_seconds = 30
"#;

    let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        TICKET_CSDL.to_string(),
        &[("Ticket", THROUGHPUT_IOA)],
    );
    let system = ActorSystem::new("throughput-1000-test");
    let state = Arc::new(ServerState::from_registry(system, registry));
    let tenant = temper_runtime::tenant::TenantId::from("default".to_string());
    let agent_ctx = AgentContext::for_service("timeout-scheduler");

    const N: usize = 1000;

    // Pre-create all entities so the dispatch phase is pure transition.
    for i in 0..N {
        state
            .get_or_create_tenant_entity(
                &tenant,
                "Ticket",
                &format!("t-{i}"),
                serde_json::json!({}),
            )
            .await
            .expect("create");
    }

    let granted = Arc::new(AtomicUsize::new(0));
    let errored = Arc::new(AtomicUsize::new(0));
    let lat_ns = Arc::new(Mutex::new(Vec::<u128>::with_capacity(N)));
    let barrier = Arc::new(tokio::sync::Barrier::new(N));

    let mut handles = Vec::with_capacity(N);
    let wall_start = Instant::now();
    for i in 0..N {
        let state = state.clone();
        let tenant = tenant.clone();
        let agent_ctx = agent_ctx.clone();
        let granted = granted.clone();
        let errored = errored.clone();
        let lat_ns = lat_ns.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let start = Instant::now();
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
            let elapsed = start.elapsed().as_nanos();
            lat_ns.lock().unwrap().push(elapsed);
            match res {
                Ok(r) if r.success => {
                    granted.fetch_add(1, Ordering::AcqRel);
                }
                _ => {
                    errored.fetch_add(1, Ordering::AcqRel);
                }
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let wall = wall_start.elapsed();

    let g = granted.load(Ordering::Acquire);
    let e = errored.load(Ordering::Acquire);
    let mut lats = lat_ns.lock().unwrap().clone();
    lats.sort_unstable();
    let p = |q: f64| -> u128 {
        let idx = ((lats.len() as f64 - 1.0) * q).round() as usize;
        lats[idx.min(lats.len() - 1)]
    };
    let tp = N as f64 / wall.as_secs_f64();

    eprintln!("1000-THROUGHPUT: granted={g} other={e} total={N}");
    eprintln!(
        "1000-PERF:   wall={wall_ms:.1}ms throughput={tp:.0} dispatches/sec",
        wall_ms = wall.as_secs_f64() * 1000.0,
    );
    eprintln!(
        "             p50={p50:.2}ms p90={p90:.2}ms p95={p95:.2}ms p99={p99:.2}ms max={pmax:.2}ms",
        p50 = (p(0.50) as f64) / 1_000_000.0,
        p90 = (p(0.90) as f64) / 1_000_000.0,
        p95 = (p(0.95) as f64) / 1_000_000.0,
        p99 = (p(0.99) as f64) / 1_000_000.0,
        pmax = (p(1.0) as f64) / 1_000_000.0,
    );

    assert_eq!(e, 0, "1000-dispatch baseline must be zero-error");
    assert_eq!(g, N, "every dispatch should succeed under generous cap");
}
