use super::*;

use crate::state::dispatch::effects::PostDispatchContext;

#[tokio::test(start_paused = true)]
async fn deleted_composite_retains_its_inactive_fence_until_actor_drain() {
    let seed = 245;
    let (_guard, clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "deleted-composite-retains-inactive-fence";
    let persistence_id = format!("default:TimedChild:{entity_id}");
    let (state, _fail_writes) = state_with_projection_failure_control(
        store.clone(),
        "deleted-composite-retains-inactive-fence",
        PARENT_DELETES_EXISTING_CHILD_IOA,
        TIMED_CHILD_DELETES_IOA,
        false,
    );

    let stale_response = state
        .get_or_create_tenant_entity(&tenant, "TimedChild", entity_id, json!({}))
        .await
        .expect("pre-create the timed composite target");
    let actor_uid = state
        .actor_registry
        .read()
        .expect("actor registry lock")
        .get(&persistence_id)
        .expect("the timed target is materialized")
        .id()
        .uid;

    let composite_params = json!({
        "sub_writes": [{
            "entity_type": "TimedChild",
            "entity_id": entity_id,
            "action": "Delete",
            "params": {}
        }]
    });
    let composite_agent = AgentContext::for_service("delete-during-delayed-callback");
    store.inject_append_batch_delay(&persistence_id, std::time::Duration::from_secs(10));
    let composite = state.apply_composite_integration_result(
        &tenant,
        "Parent",
        "parent-deletes-draining-child",
        "DeleteTimedChild",
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
            result = &mut composite => panic!("composite finished before its controlled batch delay: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(store.pending_append_batch_delays(&persistence_id), 0);

    store.inject_append_delay(&persistence_id, std::time::Duration::from_secs(20));
    let actor_agent = AgentContext::for_service("delayed-pre-delete-callback");
    let actor_action = state.dispatch_tenant_action(
        &tenant,
        "TimedChild",
        entity_id,
        "Touch",
        json!({}),
        &actor_agent,
    );
    tokio::pin!(actor_action);
    for _ in 0..128 {
        if store.pending_append_delays(&persistence_id) == 0 {
            break;
        }
        tokio::select! {
            biased;
            result = &mut actor_action => panic!("actor append finished before the deletion race: {result:?}"),
            result = &mut composite => panic!("composite finished before time advanced: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(store.pending_append_delays(&persistence_id), 0);

    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    clock.advance_by(100);
    for _ in 0..128 {
        let delete_is_durable = store
            .dump_journal(&persistence_id)
            .last()
            .is_some_and(|event| event.event_type == "Deleted");
        let stop_is_queued = state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .get(&persistence_id)
            .is_some_and(|actor_ref| actor_ref.mailbox_depth() == 1);
        if delete_is_durable && stop_is_queued {
            break;
        }
        tokio::select! {
            biased;
            result = &mut composite => panic!("composite finished before the stale actor drain: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Deleted"],
        "the deletion must be durable while the old actor is still draining"
    );

    let waiting_reader = state.get_tenant_entity_state(&tenant, "TimedChild", entity_id);
    tokio::pin!(waiting_reader);
    tokio::select! {
        biased;
        result = &mut waiting_reader => {
            panic!("a reader bypassed the composite deletion drain fence: {result:?}")
        }
        () = tokio::task::yield_now() => {}
    }

    let action_params = json!({});
    let stale_agent = AgentContext::for_service("delayed-pre-delete-callback");
    state.arm_state_timeouts_if_needed(
        &PostDispatchContext {
            tenant: &tenant,
            entity_type: "TimedChild",
            entity_id,
            action: "__delayed_pre_delete_callback",
            agent_ctx: &stale_agent,
            dispatch_idempotency_key: None,
            action_params: &action_params,
            await_integration: false,
            actor_uid: Some(actor_uid),
        },
        &stale_response,
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedChild".to_string(), 0)],
        "the synthetic deletion fence must reject a delayed callback until UID eviction"
    );

    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    clock.advance_by(100);
    assert!(
        composite
            .await
            .expect("the composite deletion completes after the actor drains")
    );
    let _ = actor_action.await;
    assert!(
        waiting_reader.await.is_err(),
        "a reader released by the drain must not publish a Deleted actor"
    );
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.size() == 0 {
            break;
        }
    }
    assert_eq!(state.state_timeout_tracker.size(), 0);
    assert!(
        !state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&persistence_id)
    );
    assert!(
        !state.entity_exists(&tenant, "TimedChild", entity_id),
        "a reader waiting on deletion must not restore index membership"
    );

    let (restarted, _fail_writes) = state_with_projection_failure_control(
        store,
        "deleted-composite-cold-restart",
        PARENT_DELETES_EXISTING_CHILD_IOA,
        TIMED_CHILD_DELETES_IOA,
        false,
    );
    restarted.hydrate_from_store(&tenant).await;
    assert!(
        !restarted.entity_exists(&tenant, "TimedChild", entity_id),
        "cold-start hydration must not re-index a composite tombstone"
    );
    assert!(
        restarted
            .list_entity_ids_lazy(&tenant, "TimedChild")
            .await
            .is_empty(),
        "typed cold-start listing must exclude a composite tombstone"
    );
}
