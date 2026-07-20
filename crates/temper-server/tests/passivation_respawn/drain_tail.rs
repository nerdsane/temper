//! FIFO-drain authoritative-tail passivation regressions.

use super::common;
use super::timeout_anchors::TIMED_TASK_IOA;
use temper_runtime::scheduler::{install_deterministic_context, sim_now};
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

#[tokio::test(start_paused = true)]
async fn concurrent_timed_transition_is_not_stranded_by_passivation() {
    let seed = 243;
    let (_guard, clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "passivation-races-timed-entry";
    let actor_key = format!("{tenant}:TimedTask:{entity_id}");
    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "passivation-races-timed-entry",
        "default",
        &[("TimedTask", TIMED_TASK_IOA)],
    );

    state
        .get_or_create_tenant_entity(&tenant, "TimedTask", entity_id, serde_json::json!({}))
        .await
        .expect("create the initially untimed task");
    state
        .last_accessed
        .write()
        .expect("last-accessed lock")
        .insert(
            actor_key.clone(),
            sim_now() - chrono::Duration::seconds(600),
        );

    sim_store.inject_snapshot_delay(&actor_key, std::time::Duration::from_secs(1));
    let passivation = state.passivate_idle_actors();
    tokio::pin!(passivation);
    for _ in 0..128 {
        if sim_store.pending_snapshot_delays(&actor_key) == 0 {
            break;
        }
        tokio::select! {
            biased;
            () = &mut passivation => panic!("passivation completed before the snapshot delay"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(sim_store.pending_snapshot_delays(&actor_key), 0);

    sim_store.inject_append_delay(&actor_key, std::time::Duration::from_secs(2));
    let timed_entry = common::dispatch(
        &state,
        &tenant,
        "TimedTask",
        entity_id,
        "Start",
        serde_json::json!({}),
    );
    tokio::pin!(timed_entry);
    for _ in 0..128 {
        if sim_store.pending_append_delays(&actor_key) == 0 {
            break;
        }
        tokio::select! {
            biased;
            result = &mut timed_entry => panic!("timed entry completed before its append delay: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(sim_store.pending_append_delays(&actor_key), 0);
    // Model the boundary where passivation has just revalidated the old access
    // timestamp and this action is admitted immediately afterward. The actor
    // has already entered its delayed durable append, so Stop must queue behind
    // it and authoritative tail replay must recover the resulting timer before
    // the actor is removed.
    state
        .last_accessed
        .write()
        .expect("last-accessed lock")
        .insert(
            actor_key.clone(),
            sim_now() - chrono::Duration::seconds(600),
        );

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::select! {
        biased;
        () = &mut passivation => panic!("passivation returned before the admitted action drained"),
        () = tokio::task::yield_now() => {}
    }
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    passivation.await;
    assert!(
        !state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key),
        "the idle actor should be removed only after its durable tail is reconciled"
    );
    let entered = timed_entry.await.expect("the timed entry commits");
    assert!(entered.success);
    assert_eq!(entered.state.status, "Running");
    assert_eq!(
        sim_store
            .dump_journal(&actor_key)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Start"]
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedTask".to_string(), 1)],
        "the committed timed entry must retain an owned deadline after passivation"
    );

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    clock.advance_by(600);
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if sim_store
            .dump_journal(&actor_key)
            .iter()
            .any(|event| event.event_type == "TimeoutFail")
        {
            break;
        }
    }
    assert_eq!(
        sim_store
            .dump_journal(&actor_key)
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Start", "TimeoutFail"],
        "passivation must not require later traffic to recover the deadline"
    );
}

#[tokio::test(start_paused = true)]
async fn passivation_replay_applies_a_concurrent_durable_delete_to_the_index() {
    let seed = 244;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "passivation-races-delete";
    let actor_key = format!("{tenant}:TimedTask:{entity_id}");
    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "passivation-races-delete",
        "default",
        &[("TimedTask", TIMED_TASK_IOA)],
    );

    state
        .get_or_create_tenant_entity(&tenant, "TimedTask", entity_id, serde_json::json!({}))
        .await
        .expect("create the entity before passivation");
    state
        .last_accessed
        .write()
        .expect("last-accessed lock")
        .insert(
            actor_key.clone(),
            sim_now() - chrono::Duration::seconds(600),
        );

    sim_store.inject_snapshot_delay(&actor_key, std::time::Duration::from_secs(1));
    let mut passivation = Box::pin(state.passivate_idle_actors());
    for _ in 0..128 {
        if sim_store.pending_snapshot_delays(&actor_key) == 0 {
            break;
        }
        tokio::select! {
            biased;
            () = &mut passivation => panic!("passivation completed before the snapshot delay"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(sim_store.pending_snapshot_delays(&actor_key), 0);

    sim_store.inject_append_delay(&actor_key, std::time::Duration::from_secs(2));
    let mut deletion = Box::pin(state.delete_tenant_entity(&tenant, "TimedTask", entity_id));
    for _ in 0..128 {
        if sim_store.pending_append_delays(&actor_key) == 0 {
            break;
        }
        tokio::select! {
            biased;
            result = &mut deletion => panic!("delete completed before its append delay: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(sim_store.pending_append_delays(&actor_key), 0);
    state
        .last_accessed
        .write()
        .expect("last-accessed lock")
        .insert(
            actor_key.clone(),
            sim_now() - chrono::Duration::seconds(600),
        );

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::select! {
        biased;
        () = &mut passivation => panic!("passivation returned before the admitted delete drained"),
        () = tokio::task::yield_now() => {}
    }
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    passivation.await;

    assert!(
        !state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key),
        "passivation removes the incarnation after replaying the durable delete"
    );
    assert_eq!(
        sim_store
            .dump_journal(&actor_key)
            .last()
            .map(|event| event.event_type.as_str()),
        Some("Deleted"),
        "the delete must be durable before passivation cleanup"
    );
    assert!(
        !state.entity_exists(&tenant, "TimedTask", entity_id),
        "authoritative Deleted replay must remove the entity from the collection index"
    );

    let deleted = deletion
        .await
        .expect("the admitted delete returns successfully");
    assert!(deleted.success);
    assert_eq!(deleted.state.status, "Deleted");
}
