//! Strict hydration must reject durable facts it cannot interpret.

use super::{INITIAL_TIMED_TASK_IOA, common};
use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, EventMetadata, EventStore, PersistenceEnvelope,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

#[tokio::test]
async fn incompatible_tail_after_snapshot_does_not_publish_or_repair_stale_timed_state() {
    let seed = 212;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "legacy-initial-timed-incompatible-tail";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");

    sim_store
        .append(
            &actor_key,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: COMPOSITE_EVENT_TYPE.to_string(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: actor_key.clone(),
                },
            }],
        )
        .await
        .expect("seed the snapshot boundary sequence");
    let legacy_snapshot = serde_json::json!({
        "entity_type": "InitialTimedTask",
        "entity_id": entity_id,
        "status": "Running",
        "item_count": 0,
        "counters": {},
        "booleans": {},
        "lists": {},
        "fields": {"Id": entity_id, "Status": "Running"},
        "total_event_count": 0,
        "events_since_snapshot": 0,
        "last_snapshot_sequence_nr": 1,
        "sequence_nr": 1,
        "processed_idempotency_keys": {}
    });
    let legacy_snapshot_bytes = serde_json::to_vec(&legacy_snapshot).unwrap();
    sim_store
        .save_snapshot(&actor_key, 1, &legacy_snapshot_bytes)
        .await
        .expect("seed legacy timed snapshot");
    sim_store
        .append(
            &actor_key,
            1,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "LegacyExit".to_string(),
                payload: serde_json::json!({"legacy_shape": true}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: actor_key.clone(),
                },
            }],
        )
        .await
        .expect("seed an incompatible tail that may have exited the timed state");

    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "legacy-timeout-incompatible-snapshot-tail",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    assert!(
        state
            .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
            .await
            .is_err(),
        "hydration must reject an incompatible durable tail instead of publishing snapshot state"
    );
    assert!(state.state_timeout_tracker.pending_snapshot().is_empty());
    assert_eq!(
        sim_store.load_snapshot(&actor_key).await.unwrap(),
        Some((1, legacy_snapshot_bytes)),
        "an incompatible tail must not authorize timeout-anchor repair"
    );
    assert_eq!(sim_store.read_events(&actor_key, 0).await.unwrap().len(), 2);
}

#[tokio::test]
async fn invalid_snapshot_does_not_fall_back_to_a_replayable_journal() {
    let seed = 213;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "invalid-snapshot-replayable-journal";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");

    sim_store
        .append(
            &actor_key,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "TimeoutFail".to_string(),
                payload: serde_json::json!({
                    "action": "TimeoutFail",
                    "from_status": "Running",
                    "to_status": "TimedOut",
                    "timestamp": sim_now(),
                    "params": {}
                }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: actor_key.clone(),
                },
            }],
        )
        .await
        .expect("seed replayable journal history");
    sim_store
        .save_snapshot(&actor_key, 1, b"not-json")
        .await
        .expect("seed an unreadable current snapshot boundary");

    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "invalid-snapshot-replayable-journal",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    assert!(
        state
            .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
            .await
            .is_err(),
        "strict hydration must not silently discard an unreadable snapshot boundary"
    );
    assert!(state.state_timeout_tracker.pending_snapshot().is_empty());
    assert_eq!(
        sim_store.load_snapshot(&actor_key).await.unwrap(),
        Some((1, b"not-json".to_vec()))
    );
}
