use super::*;

const PARENT_CREATES_UNTIMED_CHILD_IOA: &str = r#"
[automaton]
name = "Parent"
states = ["Active"]
initial = "Active"

[[action]]
name = "CreateUntimedChild"
kind = "Composite"
from = ["Active"]
to = "Active"
record_parent_event = false

[[action.sub_writes]]
target_entity = "TimedChild"
action = "Create"
generated_from = "timed_child"
"#;

const CHILD_CREATE_EXITS_TIMED_STATE_IOA: &str = r#"
[automaton]
name = "TimedChild"
states = ["Open", "Closed", "TimedOut"]
initial = "Open"
allow_indefinite_states = ["Closed", "TimedOut"]

[[action]]
name = "Create"
kind = "input"
from = ["Open"]
to = "Closed"

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Open"]
to = "TimedOut"

[[state_timeout]]
state = "Open"
after_seconds = 60
on_timeout = "TimeoutFail"
"#;

#[tokio::test(start_paused = true)]
async fn new_composite_target_evicts_an_actor_missing_from_the_preflight_snapshot() {
    let seed = 241;
    let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "actor-materialized-after-preflight";
    let persistence_id = format!("default:TimedChild:{entity_id}");
    let (state, _fail_writes) = state_with_projection_failure_control(
        store.clone(),
        "composite-preflight-actor-race",
        PARENT_CREATES_UNTIMED_CHILD_IOA,
        CHILD_CREATE_EXITS_TIMED_STATE_IOA,
        false,
    );
    let stale_actor_uid = state
        .get_or_spawn_tenant_actor(&tenant, "TimedChild", entity_id)
        .expect("production actor spawn succeeds before the composite append")
        .id()
        .uid;
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.pending_snapshot() == vec![("TimedChild".to_string(), 1)] {
            break;
        }
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedChild".to_string(), 1)],
        "the actor missing from preflight owns a stale Open-state timeout"
    );
    state
        .entity_index
        .write()
        .expect("entity index lock")
        .get_mut("default:TimedChild")
        .expect("actor spawn indexes the entity")
        .remove(entity_id);
    assert!(
        !state.entity_exists(&tenant, "TimedChild", entity_id),
        "the preflight snapshot still classifies the target as absent"
    );

    assert!(
        state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-creates-racing-child",
                "CreateUntimedChild",
                &json!({
                    "sub_writes": [{
                        "entity_type": "TimedChild",
                        "entity_id": entity_id,
                        "action": "Create",
                        "params": {}
                    }]
                }),
                &AgentContext::for_service("composite-preflight-actor-race-test"),
            )
            .await
            .expect("the absent-target deletion commits atomically")
    );
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Create"]
    );
    assert_ne!(
        state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .get(&persistence_id)
            .map(|current| current.id().uid),
        Some(stale_actor_uid),
        "the pre-commit actor incarnation must not survive the durable append"
    );
    assert!(state.entity_exists(&tenant, "TimedChild", entity_id));
    let committed = state
        .get_tenant_entity_state(&tenant, "TimedChild", entity_id)
        .await
        .expect("the current actor hydrates the post-commit state");
    assert_eq!(committed.state.status, "Closed");
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.pending_snapshot() == vec![("TimedChild".to_string(), 0)] {
            break;
        }
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedChild".to_string(), 0)],
        "the stale actor's startup timeout must retire immediately"
    );
    assert_eq!(
        state.state_timeout_tracker.size(),
        1,
        "the replacement actor retains an inactive high-water mark for delayed hydration"
    );
}
