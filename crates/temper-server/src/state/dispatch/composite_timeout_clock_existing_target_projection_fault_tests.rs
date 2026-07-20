use super::*;

use temper_spec::automaton::StateTimeout;

use crate::entity_actor::{StateTimeoutPrecondition, types::STATE_TIMEOUT_PRECONDITION_MISMATCH};
use crate::state::QueryProjectionWriteQueue;
use crate::state::dispatch::DispatchCommand;

const PARENT_ARMS_EXISTING_CHILD_IOA: &str = r#"
[automaton]
name = "Parent"
states = ["Active"]
initial = "Active"

[[action]]
name = "ArmTimedChild"
kind = "Composite"
from = ["Active"]
to = "Active"
record_parent_event = false

[[action.sub_writes]]
target_entity = "TimedChild"
action = "Arm"
generated_from = "timed_child"
"#;

const CHILD_ENTERS_TIMED_STATE_IOA: &str = r#"
[automaton]
name = "TimedChild"
states = ["Idle", "Open", "TimedOut"]
initial = "Idle"
allow_indefinite_states = ["Idle", "TimedOut"]

[[action]]
name = "Arm"
kind = "input"
from = ["Idle"]
to = "Open"

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

const PARENT_DELETES_EXISTING_CHILD_IOA: &str = r#"
[automaton]
name = "Parent"
states = ["Active"]
initial = "Active"

[[action]]
name = "DeleteTimedChild"
kind = "Composite"
from = ["Active"]
to = "Active"
record_parent_event = false

[[action.sub_writes]]
target_entity = "TimedChild"
action = "Delete"
generated_from = "timed_child"
"#;

const TIMED_CHILD_DELETES_IOA: &str = r#"
[automaton]
name = "TimedChild"
states = ["Open", "TimedOut", "Deleted"]
initial = "Open"
allow_indefinite_states = ["TimedOut", "Deleted"]

[[action]]
name = "Delete"
kind = "input"
from = ["Open"]
to = "Deleted"

[[action]]
name = "Touch"
kind = "input"
from = ["Open"]
to = "Open"

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
async fn existing_composite_deletion_cancels_timeout_when_projection_fails_after_commit() {
    let seed = 238;
    let (_guard, clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "existing-timed-composite-is-deleted";
    let persistence_id = format!("default:TimedChild:{entity_id}");
    let (state, fail_writes) = state_with_projection_failure_control(
        store.clone(),
        "deleted-composite-projection-failure",
        PARENT_DELETES_EXISTING_CHILD_IOA,
        TIMED_CHILD_DELETES_IOA,
        false,
    );

    let created = state
        .get_or_create_tenant_entity(&tenant, "TimedChild", entity_id, json!({}))
        .await
        .expect("pre-create the timed composite target");
    let stale_timeout_precondition = StateTimeoutPrecondition {
        expected_timeout: StateTimeout {
            state: "Open".to_string(),
            after_seconds: 60,
            on_timeout: "TimeoutFail".to_string(),
            max_occurrences: 1,
            reset_on: Vec::new(),
            params: BTreeMap::new(),
        },
        expected_state: "Open".to_string(),
        expected_reset_at: created.state.state_timeout_clock_reset_at,
        expected_reset_version: created.state.state_timeout_clock_reset_version,
    };
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedChild".to_string(), 1)],
        "the pre-existing Open target starts with one timeout"
    );
    assert!(
        state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&persistence_id),
        "the timed target starts materialized"
    );
    fail_writes.store(true, Ordering::SeqCst);

    let error = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-deletes-existing-child",
            "DeleteTimedChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "TimedChild",
                    "entity_id": entity_id,
                    "action": "Delete",
                    "params": {}
                }]
            }),
            &AgentContext::for_service("deleted-composite-projection-failure-test"),
        )
        .await
        .expect_err("the injected query projection write must fail");
    assert!(
        error
            .to_string()
            .contains("query projection removal failed after composite batch"),
        "unexpected post-commit failure: {error}"
    );
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Deleted"],
        "the deletion commits before the injected projection failure"
    );
    assert!(
        !state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&persistence_id),
        "the durably deleted target must not retain its stale actor"
    );
    assert!(
        !state.entity_exists(&tenant, "TimedChild", entity_id),
        "the durably deleted target must stay out of the live entity index"
    );
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.pending_snapshot() == vec![("TimedChild".to_string(), 0)] {
            break;
        }
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedChild".to_string(), 0)],
        "the pre-delete timer task must retire immediately without waiting for its deadline"
    );
    assert_eq!(
        state.state_timeout_tracker.size(),
        0,
        "the evicted deleted entity must not retain a timeout ownership tombstone"
    );

    let timeout_agent = AgentContext::for_service("in-flight-timeout-after-delete-test");
    let cancellation = state
        .dispatch_state_timeout_action(
            DispatchCommand {
                tenant: &tenant,
                entity_type: "TimedChild",
                entity_id,
                action: "TimeoutFail",
                params: json!({}),
                agent_ctx: &timeout_agent,
                await_integration: false,
                await_reactions: true,
            },
            stale_timeout_precondition,
        )
        .await
        .expect("the already-admitted stale timeout returns a cancellation");
    assert_eq!(
        cancellation.error.as_deref(),
        Some(STATE_TIMEOUT_PRECONDITION_MISMATCH),
        "the durable deletion must cancel the in-flight stale timeout"
    );
    assert!(
        !state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&persistence_id),
        "a cancelled in-flight timeout must not retain the rematerialized deleted actor"
    );
    assert!(
        !state.entity_exists(&tenant, "TimedChild", entity_id),
        "a cancelled in-flight timeout must not reinsert the durably deleted entity"
    );

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    clock.advance_by(600);
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Deleted"],
        "the cancelled pre-delete timeout must never append"
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedChild".to_string(), 0)],
        "the invalidated pre-delete timer must remain retired at its old deadline"
    );
    assert!(
        !state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&persistence_id),
        "the cancelled timeout must not respawn a deleted actor"
    );
    assert!(
        !state.entity_exists(&tenant, "TimedChild", entity_id),
        "the cancelled timeout must not reinsert a deleted entity"
    );
}

#[tokio::test(start_paused = true)]
async fn existing_composite_deletion_queues_projection_removal() {
    let seed = 240;
    let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "queued-composite-deletion";
    let (state, fail_writes) = state_with_projection_failure_control(
        store,
        "queued-composite-deletion",
        PARENT_DELETES_EXISTING_CHILD_IOA,
        TIMED_CHILD_DELETES_IOA,
        false,
    );
    let queue = Arc::new(QueryProjectionWriteQueue::new_for_test(
        Arc::new(FailingQueryPlane { fail_writes }),
        16,
        16,
    ));
    *state
        .query_projection_queue
        .lock()
        .expect("query projection queue lock") = Some(queue.clone());
    state
        .get_or_create_tenant_entity(&tenant, "TimedChild", entity_id, json!({}))
        .await
        .expect("pre-create the queued deletion target");

    assert!(
        state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "parent-queues-child-deletion",
                "DeleteTimedChild",
                &json!({
                    "sub_writes": [{
                        "entity_type": "TimedChild",
                        "entity_id": entity_id,
                        "action": "Delete",
                        "params": {}
                    }]
                }),
                &AgentContext::for_service("queued-composite-deletion-test"),
            )
            .await
            .expect("the durable deletion is accepted into the projection queue")
    );
    assert_eq!(
        queue.pending_update_for_test("default", "TimedChild", entity_id),
        Some(("remove", 2, "queued_composite")),
        "the queued composite path must remove rather than upsert the deleted row"
    );
}

#[tokio::test(start_paused = true)]
async fn existing_composite_target_arms_timeout_when_projection_fails_after_timed_entry() {
    let seed = 237;
    let (_guard, clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "existing-composite-enters-timed-state";
    let persistence_id = format!("default:TimedChild:{entity_id}");
    let (state, fail_writes) = state_with_projection_failure_control(
        store.clone(),
        "existing-composite-projection-failure",
        PARENT_ARMS_EXISTING_CHILD_IOA,
        CHILD_ENTERS_TIMED_STATE_IOA,
        false,
    );

    state
        .get_or_create_tenant_entity(&tenant, "TimedChild", entity_id, json!({}))
        .await
        .expect("pre-create the untimed composite target");
    assert!(
        state.state_timeout_tracker.pending_snapshot().is_empty(),
        "the pre-existing Idle target has no timeout"
    );
    fail_writes.store(true, Ordering::SeqCst);

    let error = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-arms-existing-child",
            "ArmTimedChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "TimedChild",
                    "entity_id": entity_id,
                    "action": "Arm",
                    "params": {}
                }]
            }),
            &AgentContext::for_service("existing-composite-projection-failure-test"),
        )
        .await
        .expect_err("the injected query projection write must fail");
    assert!(
        error
            .to_string()
            .contains("query projection write failed after composite batch"),
        "unexpected post-commit failure: {error}"
    );
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Arm"],
        "the timed-state entry commits before the injected projection failure"
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedChild".to_string(), 1)],
        "a pre-existing target entering a timed state must arm before projection"
    );
    assert!(
        state.entity_exists(&tenant, "TimedChild", entity_id),
        "the durably live target must remain indexed despite projection failure"
    );
    assert!(
        state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&persistence_id),
        "the authoritative replacement actor must hydrate before projection failure surfaces"
    );

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    clock.advance_by(600);
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if store
            .dump_journal(&persistence_id)
            .iter()
            .any(|event| event.event_type == "TimeoutFail")
        {
            break;
        }
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Arm", "TimeoutFail"],
        "the existing target must time out without retry, access, or restart"
    );

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    clock.advance_by(600);
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .filter(|event| event.event_type == "TimeoutFail")
            .count(),
        1,
        "the pre-existing projection-fault path must deliver exactly once"
    );
}

#[path = "composite_timeout_clock_preflight_actor_race_tests.rs"]
mod preflight_actor_race_tests;

#[path = "composite_timeout_clock_inflight_actor_race_tests.rs"]
mod inflight_actor_race_tests;

#[path = "composite_timeout_clock_inactive_fence_tests.rs"]
mod inactive_fence_tests;
