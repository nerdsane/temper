//! Production detached handoffs must not retain a live inline dependency.
use super::*;
use temper_server::idempotency::{IdempotencyLookup, request_binding};
use temper_server::request_context::MAX_CALLBACK_HOPS;

fn register(state: &ServerState, specs: &[(&str, &str)]) {
    state.registry.write().unwrap().register_tenant(
        "default",
        parse_csdl(TASK_CSDL).unwrap(),
        TASK_CSDL.into(),
        specs,
    );
}

#[tokio::test(start_paused = true)]
async fn detached_state_timer_joins_owned_work_after_caller_cancellation() {
    let (_guard, _, _) = install_deterministic_context(51960);
    let (state, store, adapter) = fixture(51960);
    let spec = SPEC
        .replace("after_seconds = 60", "after_seconds = 1")
        .replace("on_timeout = \"TimeoutFail\"", "on_timeout = \"Start\"");
    register(&state, &[("TimedTask", &spec)]);
    let mut owner = Box::pin(start(&state));
    tokio::select! { biased;
        response = &mut owner => panic!("owner returned before adapter: {response:?}"),
        _ = adapter.started.notified() => {}
    }
    // Let the actual timer task register its sleep, then fire while the owner
    // is gated. Its identical actor/key/body is independent, not an ancestor.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedTask".into(), 1)],
        "detached timer exited instead of waiting for the effects owner"
    );
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    drop(owner);
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    adapter.finish.notify_one();
    tokio::time::timeout(Duration::from_secs(1), start(&state))
        .await
        .unwrap();
    let binding = request_binding("Start", &json!({}));
    assert!(
        matches!(
            state.idempotency_cache.lookup_after_completion(
                "default:TimedTask:task",
                "same-key",
                &binding
            ),
            IdempotencyLookup::Hit(_)
        ),
        "timer did not join the server-owned completion"
    );
    assert_eq!(
        store
            .dump_journal("default:TimedTask:task")
            .iter()
            .filter(|e| e.payload["action"] == "Start")
            .count(),
        1
    );
    adapter.finish.notify_one();
}

fn near_bound_context() -> AgentContext {
    AgentContext {
        effects_ancestors: (0..MAX_CALLBACK_HOPS - 1)
            .map(|n| (format!("prior:Task:{n}"), "prior-key".into()))
            .collect(),
        callback_depth: 3,
        callback_hops: 17,
        session_id: Some("detached-session".into()),
        intent: Some("detached-chain".into()),
        ..AgentContext::system()
    }
}

async fn timer_chain_ignores_completed_ancestors(state_timer: bool, seed: u64) {
    let (_guard, _, _) = install_deterministic_context(seed);
    let (state, _, _) = fixture(seed);
    let effects = if state_timer {
        "effect = [\"ticks += 1\"]"
    } else {
        "effect = [\"ticks += 1\", \"schedule('Tick', 1)\"]"
    };
    let timeout = if state_timer {
        r#"
[[state_timeout]]
state = "Running"
after_seconds = 1
on_timeout = "Tick"
reset_on = ["Tick"]
"#
    } else {
        ""
    };
    let spec = format!(
        r#"
[automaton]
name = "TimedTask"
states = ["Running"]
initial = "Running"
[[state]]
name = "ticks"
type = "counter"
initial = 0
[[action]]
name = "Tick"
from = ["Running"]
to = "Running"
{effects}
{timeout}
"#
    );
    register(&state, &[("TimedTask", &spec)]);
    let agent = near_bound_context();
    let first = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "TimedTask",
            "chain",
            "Tick",
            json!({}),
            &agent,
        )
        .await
        .unwrap();
    assert!(first.success, "{first:?}");
    let mut changes = state.event_tx.subscribe();
    for expected_ticks in 2..=5 {
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let changed = tokio::time::timeout(Duration::from_millis(100), changes.recv())
            .await
            .expect("detached timer chain retained completed ancestors and stopped its effects")
            .unwrap();
        assert_eq!(changed.action, "Tick");
        assert_eq!(changed.agent_id, agent.agent_id);
        assert_eq!(changed.session_id, agent.session_id);
        assert_eq!(changed.intent, agent.intent);
        let current = state
            .get_tenant_entity_state(&TenantId::default(), "TimedTask", "chain")
            .await
            .unwrap();
        assert_eq!(current.state.counters["ticks"], expected_ticks);
    }
}

#[tokio::test(start_paused = true)]
async fn detached_scheduled_chain_does_not_accumulate_completed_ancestors() {
    timer_chain_ignores_completed_ancestors(false, 51961).await;
}

#[tokio::test(start_paused = true)]
async fn detached_state_timer_chain_does_not_accumulate_completed_ancestors() {
    timer_chain_ignores_completed_ancestors(true, 51962).await;
}

#[tokio::test(start_paused = true)]
async fn detached_spawn_chain_does_not_accumulate_completed_ancestors() {
    let (_guard, _, _) = install_deterministic_context(51963);
    let (state, store, _) = fixture(51963);
    let spec = |name: &str, effect: &str| {
        format!(
            r#"
[automaton]
name = "{name}"
states = ["Idle", "Ready"]
initial = "Idle"
[[action]]
name = "Initialize"
from = ["Idle"]
to = "Ready"
params = ["child_id"]
{effect}
"#
        )
    };
    let root = spec(
        "TimedTask",
        "effect = [\"spawn('Child', 'Initialize', child_ref, params.child_id)\"]",
    );
    let child = spec(
        "Child",
        "effect = [\"spawn('Grandchild', 'Initialize', child_ref, params.child_id)\"]",
    );
    let grandchild = spec("Grandchild", "");
    register(
        &state,
        &[
            ("TimedTask", &root),
            ("Child", &child),
            ("Grandchild", &grandchild),
        ],
    );
    let agent = near_bound_context();
    let mut changes = state.event_tx.subscribe();
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "TimedTask",
            "root",
            "Initialize",
            json!({"child_id":"descendant"}),
            &agent,
        )
        .await
        .unwrap();
    assert!(response.success, "{response:?}");
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let change = changes.recv().await.unwrap();
            if change.action != "Initialize" {
                continue;
            }
            assert_eq!(change.agent_id, agent.agent_id);
            assert_eq!(change.session_id, agent.session_id);
            if change.entity_type == "Grandchild" && change.action == "Initialize" {
                break;
            }
        }
    })
    .await
    .expect("spawn chain retained completed ancestors and stopped its effects");
    assert_eq!(
        store
            .dump_journal("default:Grandchild:descendant")
            .iter()
            .filter(|e| e.payload["action"] == "Initialize")
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn detached_reaction_waits_independently_of_its_invoking_inline_ancestor() {
    let (_guard, _, _) = install_deterministic_context(51964);
    let (state, _, adapter) = fixture(51964);
    let parent = r#"
[automaton]
name = "Parent"
states = ["Idle"]
initial = "Idle"
[[action]]
name = "Launch"
from = ["Idle"]
[[action.triggers]]
name = "retry_source"
kind = "entity"
target_entity = "TimedTask"
target_action = "Start"
resolve_target = { kind = "same_id" }
"#;
    register(&state, &[("TimedTask", SPEC), ("Parent", parent)]);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    state.rebuild_reaction_dispatcher();
    let mut owner = Box::pin(start(&state));
    tokio::select! { biased;
        response = &mut owner => panic!("owner returned before adapter: {response:?}"),
        _ = adapter.started.notified() => {}
    }
    // Model an inline descendant launching a detached reaction back to its
    // live ancestor. Only the actual spawn (not inline fallback) may clear it.
    let agent = AgentContext {
        idempotency_key: Some("same-key".into()),
        effects_ancestors: vec![("default:TimedTask:task".into(), "same-key".into())],
        ..AgentContext::system()
    };
    let parent = state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            "Parent",
            "task",
            "Launch",
            json!({}),
            DispatchExtOptions {
                agent_ctx: &agent,
                await_integration: false,
                await_reactions: false,
            },
        )
        .await
        .unwrap();
    assert!(parent.success, "{parent:?}");
    let mut parent_terminal = Box::pin(async {
        state
            .dispatch_tenant_action_ext(
                &TenantId::default(),
                "Parent",
                "task",
                "Launch",
                json!({}),
                DispatchExtOptions {
                    agent_ctx: &agent,
                    await_integration: false,
                    await_reactions: true,
                },
            )
            .await
            .unwrap()
    });
    // A false ancestry refusal would finish the best-effort parent cascade.
    // Prove the actual detached reaction remains joined, not merely that it
    // refrained from running a second integration.
    assert_waiting(&mut parent_terminal, 51964).await;
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    drop(owner);
    assert_waiting(&mut parent_terminal, 51964).await;
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 1);
    adapter.finish.notify_one();
    let (ancestor, parent) = tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(start(&state), parent_terminal)
    })
    .await
    .unwrap();
    assert!(ancestor.success && parent.success);
}
