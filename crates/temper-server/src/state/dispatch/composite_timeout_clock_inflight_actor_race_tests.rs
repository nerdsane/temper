use super::*;

use crate::entity_actor::{EntityMsg, EntityResponse};

const PARENT_TOUCHES_EXISTING_CHILD_IOA: &str = r#"
[automaton]
name = "Parent"
states = ["Active"]
initial = "Active"

[[action]]
name = "TouchChild"
kind = "Composite"
from = ["Active"]
to = "Active"
record_parent_event = false

[[action.sub_writes]]
target_entity = "TimedChild"
action = "Touch"
generated_from = "timed_child"
"#;

const CHILD_ACTOR_ENTERS_TIMED_STATE_IOA: &str = r#"
[automaton]
name = "TimedChild"
states = ["Idle", "Open", "TimedOut"]
initial = "Idle"
allow_indefinite_states = ["Idle", "TimedOut"]

[[action]]
name = "Touch"
kind = "input"
from = ["Idle"]
to = "Idle"

[[action]]
name = "EnterTimed"
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

#[tokio::test(start_paused = true)]
async fn projection_failure_reconciles_a_newer_commit_from_the_drained_actor() {
    let seed = 242;
    let (_guard, clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "inflight-actor-enters-timed-state";
    let persistence_id = format!("default:TimedChild:{entity_id}");
    let (state, fail_writes) = state_with_projection_failure_control(
        store.clone(),
        "inflight-actor-projection-failure",
        PARENT_TOUCHES_EXISTING_CHILD_IOA,
        CHILD_ACTOR_ENTERS_TIMED_STATE_IOA,
        false,
    );

    state
        .get_or_create_tenant_entity(&tenant, "TimedChild", entity_id, json!({}))
        .await
        .expect("pre-create the untimed composite target");
    let precommit_actor = state
        .actor_registry
        .read()
        .expect("actor registry lock")
        .get(&persistence_id)
        .expect("the pre-created target is materialized")
        .clone();
    assert!(state.state_timeout_tracker.pending_snapshot().is_empty());

    fail_writes.store(true, Ordering::SeqCst);
    store.inject_append_batch_delay(&persistence_id, std::time::Duration::from_secs(10));
    store.inject_append_delay(&persistence_id, std::time::Duration::from_secs(20));
    let composite_params = json!({
        "sub_writes": [{
            "entity_type": "TimedChild",
            "entity_id": entity_id,
            "action": "Touch",
            "params": {}
        }]
    });
    let composite_agent = AgentContext::for_service("inflight-composite-projection-failure-test");
    let composite = state.apply_composite_integration_result(
        &tenant,
        "Parent",
        "parent-touches-racing-child",
        "TouchChild",
        &composite_params,
        &composite_agent,
    );
    tokio::pin!(composite);
    for _ in 0..128 {
        if store.pending_append_batch_delays(&persistence_id) == 0 {
            break;
        }
        tokio::select! {
            biased;
            result = &mut composite => panic!("composite batch finished before its controlled delay: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(
        store.pending_append_batch_delays(&persistence_id),
        0,
        "the composite must be waiting after preflight and before its OCC check"
    );

    let action_agent = AgentContext::for_service("inflight-actor-entry-test");
    let actor_action = state.dispatch_tenant_action(
        &tenant,
        "TimedChild",
        entity_id,
        "EnterTimed",
        json!({}),
        &action_agent,
    );
    tokio::pin!(actor_action);
    for _ in 0..128 {
        if store.pending_append_delays(&persistence_id) == 0 {
            break;
        }
        tokio::select! {
            biased;
            result = &mut actor_action => panic!("actor append finished before the composite race: {result:?}"),
            result = &mut composite => panic!("composite batch finished before time advanced: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(
        store.pending_append_delays(&persistence_id),
        0,
        "the actor must be waiting inside its delayed first append"
    );

    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    clock.advance_by(100);
    let mut composite_result = None;
    for _ in 0..32 {
        tokio::select! {
            biased;
            result = &mut composite => {
                composite_result = Some(result);
                break;
            }
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Touch"],
        "the composite append must win OCC while the actor append is delayed"
    );

    // Exercise the registry-publication window while the pre-commit actor is
    // still draining. Synchronous publication is fenced, while the async read
    // waits for cleanup and then resolves the authoritative replacement.
    assert!(
        state
            .get_or_spawn_tenant_actor(&tenant, "TimedChild", entity_id)
            .is_none(),
        "a draining actor must neither accept traffic nor publish a replacement"
    );
    let retry_policy = state.dispatch_retry_policy();
    let read_during_drain = state.ask_actor_with_drain_retry::<EntityResponse, _>(
        &tenant,
        "TimedChild",
        entity_id,
        precommit_actor,
        || EntityMsg::GetState,
        &retry_policy,
    );
    tokio::pin!(read_during_drain);
    tokio::select! {
        biased;
        result = &mut read_during_drain => panic!("read crossed the actor drain fence: {result:?}"),
        () = tokio::task::yield_now() => {}
    }

    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    clock.advance_by(100);
    let error = match composite_result {
        Some(result) => result,
        None => composite.await,
    }
    .expect_err("the injected query projection write must fail");
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedChild".to_string(), 1)],
        "the composite return barrier itself must publish the latest durable timeout owner"
    );
    let actor_dispatch = actor_action.await;
    if let Ok(response) = actor_dispatch {
        assert_eq!(response.state.status, "Open");
    }
    let (_, replacement_outcome) = read_during_drain.await;
    let replacement = replacement_outcome
        .result
        .expect("the fenced read retries against the authoritative replacement");
    assert_eq!(replacement.state.status, "Open");
    assert_eq!(replacement.state.sequence_nr, 3);
    assert!(
        error
            .to_string()
            .contains("query projection write failed after composite batch"),
        "unexpected post-commit failure: {error}"
    );
    fail_writes.store(false, Ordering::SeqCst);
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Touch", "EnterTimed"],
        "the in-flight actor must commit after replaying the composite append"
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
        vec!["Created", "Touch", "EnterTimed", "TimeoutFail"],
        "the newer timed commit must fire without traffic or restart"
    );
}

#[tokio::test(start_paused = true)]
async fn request_cancellation_cannot_abort_committed_composite_reconciliation() {
    let seed = 253;
    let (_guard, clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "cancelled-request-committed-child";
    let persistence_id = format!("default:TimedChild:{entity_id}");
    let (state, _fail_writes) = state_with_projection_failure_control(
        store.clone(),
        "cancelled-composite-reconciliation",
        PARENT_TOUCHES_EXISTING_CHILD_IOA,
        CHILD_ACTOR_ENTERS_TIMED_STATE_IOA,
        false,
    );

    state
        .get_or_create_tenant_entity(&tenant, "TimedChild", entity_id, json!({}))
        .await
        .expect("pre-create the untimed composite target");
    let stale_uid = state
        .actor_registry
        .read()
        .expect("actor registry lock")
        .get(&persistence_id)
        .expect("the pre-created target is materialized")
        .id()
        .uid;

    store.inject_append_batch_delay(&persistence_id, std::time::Duration::from_secs(10));
    store.inject_append_delay(&persistence_id, std::time::Duration::from_secs(20));
    let composite_state = state.clone();
    let composite_tenant = tenant.clone();
    let composite_entity_id = entity_id.to_string();
    let composite = tokio::spawn(async move {
        let params = json!({
            "sub_writes": [{
                "entity_type": "TimedChild",
                "entity_id": composite_entity_id,
                "action": "Touch",
                "params": {}
            }]
        });
        let agent = AgentContext::for_service("cancelled-composite-request");
        composite_state
            .apply_composite_integration_result(
                &composite_tenant,
                "Parent",
                "parent-cancelled-after-commit",
                "TouchChild",
                &params,
                &agent,
            )
            .await
    });
    for _ in 0..128 {
        if store.pending_append_batch_delays(&persistence_id) == 0 {
            break;
        }
        assert!(
            !composite.is_finished(),
            "composite finished before its batch delay"
        );
        tokio::task::yield_now().await;
    }
    assert_eq!(store.pending_append_batch_delays(&persistence_id), 0);

    let action_state = state.clone();
    let action_tenant = tenant.clone();
    let action_entity_id = entity_id.to_string();
    let actor_action = tokio::spawn(async move {
        let agent = AgentContext::for_service("cancelled-composite-racing-actor");
        action_state
            .dispatch_tenant_action(
                &action_tenant,
                "TimedChild",
                &action_entity_id,
                "EnterTimed",
                json!({}),
                &agent,
            )
            .await
    });
    for _ in 0..128 {
        if store.pending_append_delays(&persistence_id) == 0 {
            break;
        }
        assert!(
            !actor_action.is_finished(),
            "actor append finished before its delay"
        );
        assert!(
            !composite.is_finished(),
            "composite finished before the batch commit"
        );
        tokio::task::yield_now().await;
    }
    assert_eq!(store.pending_append_delays(&persistence_id), 0);

    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    clock.advance_by(100);
    for _ in 0..128 {
        if store.dump_journal(&persistence_id).len() == 2 {
            break;
        }
        assert!(
            !composite.is_finished(),
            "reconciliation did not wait for the stale actor"
        );
        tokio::task::yield_now().await;
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Touch"],
        "the cancellation point must be after the durable batch commit"
    );
    assert!(
        !composite.is_finished(),
        "the request must still be awaiting stale-actor reconciliation"
    );

    composite.abort();
    let cancellation = composite.await.expect_err("the request task is cancelled");
    assert!(cancellation.is_cancelled());

    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    clock.advance_by(100);
    let _ = actor_action
        .await
        .expect("racing actor task remains alive after request cancellation");
    for _ in 0..256 {
        let replacement_is_ready = state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .get(&persistence_id)
            .is_some_and(|actor| actor.id().uid != stale_uid && !actor.is_draining());
        if replacement_is_ready
            && state.state_timeout_tracker.pending_snapshot() == vec![("TimedChild".to_string(), 1)]
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedChild".to_string(), 1)],
        "the detached reconciliation must publish the committed timeout owner"
    );
    assert!(
        state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .get(&persistence_id)
            .is_some_and(|actor| actor.id().uid != stale_uid && !actor.is_draining()),
        "the detached reconciliation must replace the stale incarnation"
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
        vec!["Created", "Touch", "EnterTimed", "TimeoutFail"],
        "request cancellation cannot strand the newer durable timeout"
    );
}
