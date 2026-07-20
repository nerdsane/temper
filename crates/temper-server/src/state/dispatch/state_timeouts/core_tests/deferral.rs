//! Tight-admission timeout deferral regressions.

use super::*;

/// Tight-cap spec with `queue_timeout_seconds = 0` — acquirers that
/// cannot grab a permit instantly are immediately deferred.
/// This proves the admission gate actually enforces the cap under
/// sustained contention — a slow real-world action would see the same cap
/// enforced via a non-zero queue timeout.
const TICKET_ZERO_QUEUE_IOA: &str = r#"
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
max_concurrent_creates = 2
max_concurrent_actions = { "AssignAgent" = 2 }
queue_depth = 1000
queue_timeout_seconds = 0
"#;

/// Adversarial burst-load: 300 simultaneous dispatches against cap=2
/// with a zero-second queue budget. Without admission's FIFO gate, the
/// flood would hit the shared actor's 1000-deep mailbox and produce
/// `MailboxFull` errors (the 2026-04-17 Katagami incident pattern).
/// With admission active, the contract is: every caller is either
/// `Granted` (served quickly through the cap) or `Deferred` (503
/// Retry-After). **Zero 500s under any circumstances.**
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn load_tight_cap_observes_deferrals() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        TICKET_CSDL.to_string(),
        &[("Ticket", TICKET_ZERO_QUEUE_IOA)],
    );
    let system = ActorSystem::new("load-tight-admission-test");
    let state = Arc::new(ServerState::from_registry(system, registry));
    let tenant = temper_runtime::tenant::TenantId::from("default".to_string());
    let agent_ctx = AgentContext::for_service("timeout-scheduler");

    // Shared entity — all 300 dispatches contend for the SAME ticket
    // so the actor's single-threaded processing adds queue time on
    // top of the admission gate, making deferrals observable.
    state
        .get_or_create_tenant_entity(&tenant, "Ticket", "shared-ticket", serde_json::json!({}))
        .await
        .expect("create");

    const N: usize = 300;
    let granted = Arc::new(AtomicUsize::new(0));
    let deferred = Arc::new(AtomicUsize::new(0));
    let other = Arc::new(AtomicUsize::new(0));
    let lat_granted_ns = Arc::new(std::sync::Mutex::new(Vec::<u128>::new()));
    let lat_deferred_ns = Arc::new(std::sync::Mutex::new(Vec::<u128>::new()));

    // Prime a synchronization barrier so ALL 300 fire at the same instant.
    let barrier = Arc::new(tokio::sync::Barrier::new(N));
    let mut handles = Vec::with_capacity(N);
    let wall_start = std::time::Instant::now();
    for _i in 0..N {
        let state = state.clone();
        let tenant = tenant.clone();
        let agent_ctx = agent_ctx.clone();
        let granted = granted.clone();
        let deferred = deferred.clone();
        let other = other.clone();
        let lat_granted_ns = lat_granted_ns.clone();
        let lat_deferred_ns = lat_deferred_ns.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            let call_start = std::time::Instant::now();
            let res = state
                .dispatch_tenant_action_ext_typed(
                    &tenant,
                    "Ticket",
                    "shared-ticket",
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
            match res {
                Ok(_) => {
                    granted.fetch_add(1, Ordering::AcqRel);
                    lat_granted_ns.lock().unwrap().push(call_ns);
                }
                Err(crate::state::dispatch::DispatchError::Deferred { .. }) => {
                    deferred.fetch_add(1, Ordering::AcqRel);
                    lat_deferred_ns.lock().unwrap().push(call_ns);
                }
                Err(_) => {
                    other.fetch_add(1, Ordering::AcqRel);
                }
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let wall = wall_start.elapsed();

    let g = granted.load(Ordering::Acquire);
    let d = deferred.load(Ordering::Acquire);
    let o = other.load(Ordering::Acquire);
    let throughput = N as f64 / wall.as_secs_f64();
    let mut gl = lat_granted_ns.lock().unwrap().clone();
    let mut dl = lat_deferred_ns.lock().unwrap().clone();
    gl.sort_unstable();
    dl.sort_unstable();
    let p = |v: &[u128], q: f64| -> u128 {
        if v.is_empty() {
            return 0;
        }
        let idx = ((v.len() as f64 - 1.0) * q).round() as usize;
        v[idx.min(v.len() - 1)]
    };
    eprintln!("TIGHT-CAP RESULT: granted={g} deferred={d} other={o} total={N}");
    eprintln!(
        "TIGHT-CAP PERF:   wall={wall_ms:.1}ms throughput={tp:.0}/s",
        wall_ms = wall.as_secs_f64() * 1000.0,
        tp = throughput,
    );
    eprintln!(
        "                  granted p50={gp50:.2}ms p95={gp95:.2}ms p99={gp99:.2}ms",
        gp50 = (p(&gl, 0.50) as f64) / 1_000_000.0,
        gp95 = (p(&gl, 0.95) as f64) / 1_000_000.0,
        gp99 = (p(&gl, 0.99) as f64) / 1_000_000.0,
    );
    eprintln!(
        "                  deferred p50={dp50:.2}ms p95={dp95:.2}ms p99={dp99:.2}ms (time-to-503)",
        dp50 = (p(&dl, 0.50) as f64) / 1_000_000.0,
        dp95 = (p(&dl, 0.95) as f64) / 1_000_000.0,
        dp99 = (p(&dl, 0.99) as f64) / 1_000_000.0,
    );

    // Hard contract:
    //   * Zero 500-class outcomes. Every caller is either served or told
    //     to back off with a 503-equivalent.
    assert_eq!(o, 0, "expected no 500-class outcomes, got {o}");
    //   * All N accounted for.
    assert_eq!(g + d + o, N);
    //   * Admission actually bites: with cap=2 and 1s queue timeout
    //     against 300 contenders on one actor, not all can serve.
    //     This is the incident-class proof: burst → deferrals, not 500s.
    assert!(
        d > 0,
        "admission control must produce at least one deferral under tight cap; got granted={g} deferred={d}"
    );
}
