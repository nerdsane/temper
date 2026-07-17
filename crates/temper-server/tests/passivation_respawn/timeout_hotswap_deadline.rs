//! Timeout deadline stability across a post-snapshot spec hot-swap.

use super::common;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const INITIAL_UNTIMED_TASK_IOA: &str = r#"
[automaton]
name = "InitialTimedTask"
states = ["Running", "TimedOut"]
initial = "Running"
allow_indefinite_states = ["Running", "TimedOut"]

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"

[[action]]
name = "Observe"
kind = "input"
from = ["Running"]
to = "Running"
"#;

const INITIAL_TIMED_TASK_IOA: &str = r#"
[automaton]
name = "InitialTimedTask"
states = ["Running", "TimedOut"]
initial = "Running"
allow_indefinite_states = ["TimedOut"]

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"

[[action]]
name = "Observe"
kind = "input"
from = ["Running"]
to = "Running"

[[state_timeout]]
state = "Running"
after_seconds = 600
on_timeout = "TimeoutFail"
"#;

#[tokio::test(start_paused = true)]
async fn unrelated_event_after_post_snapshot_hotswap_preserves_original_deadline() {
    let seed = 215;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "hotswap-unrelated-event";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "hotswap-unrelated-event",
        "default",
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );

    // Capture the untimed table in pre_start, then add the timeout while the
    // Created append is held. Hydration must derive the original t=0 anchor
    // even though the actor did not establish timeout clock metadata.
    sim_store.inject_append_delay(&actor_key, std::time::Duration::from_secs(120));
    state
        .get_or_spawn_tenant_actor(&tenant, "InitialTimedTask", entity_id)
        .expect("spawn the actor under the untimed table");
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.pending_append_delays(&actor_key) == 0 {
            break;
        }
    }
    assert_eq!(sim_store.pending_append_delays(&actor_key), 0);

    {
        let mut registry = state.registry.write().expect("registry lock");
        let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
        registry.register_tenant(
            "default",
            csdl,
            common::CSDL_XML.to_string(),
            &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
        );
    }

    tokio::time::advance(std::time::Duration::from_secs(120)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 1
            && state.state_timeout_tracker.pending_snapshot()
                == vec![("InitialTimedTask".to_string(), 1)]
        {
            break;
        }
    }
    assert_eq!(sim_store.total_events(), 1);
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)]
    );

    tokio::time::advance(std::time::Duration::from_secs(240)).await;
    let observed = common::dispatch(
        &state,
        &tenant,
        "InitialTimedTask",
        entity_id,
        "Observe",
        serde_json::json!({}),
    )
    .await
    .expect("unrelated same-state action succeeds");
    assert!(observed.success);
    assert_eq!(observed.state.status, "Running");

    tokio::time::advance(std::time::Duration::from_secs(239)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        sim_store.total_events(),
        2,
        "the unrelated event must not make the original timeout fire early"
    );

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 3 {
            break;
        }
    }
    let journal = sim_store
        .read_events(&actor_key, 0)
        .await
        .expect("read the post-hot-swap journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Observe", "TimeoutFail"],
        "an unrelated event must not replace the durable Created-event deadline"
    );
}
