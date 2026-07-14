//! Journal-read failures must not authorize a legacy snapshot repair.

use super::{INITIAL_TIMED_TASK_IOA, common};
use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, EventMetadata, EventStore, PersistenceEnvelope,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

#[tokio::test]
async fn journal_tail_read_failure_does_not_repair_or_arm_stale_timed_state() {
    let seed = 210;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "legacy-initial-timed-unreadable-tail";
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
    let legacy_snapshot_bytes =
        serde_json::to_vec(&legacy_snapshot).expect("legacy snapshot serialization");
    sim_store
        .save_snapshot(&actor_key, 1, &legacy_snapshot_bytes)
        .await
        .expect("seed a legacy snapshot without a timeout anchor");
    sim_store
        .append(
            &actor_key,
            1,
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
        .expect("seed a post-snapshot event that exits the timed state");
    let segments_before = sim_store.dump_segments(&actor_key);
    sim_store.fail_next_reads(&actor_key, 1);

    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "legacy-timeout-unreadable-journal-tail",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    assert!(
        state
            .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
            .await
            .is_err(),
        "hydration must fail when it cannot prove the snapshot tail was replayed"
    );
    assert!(
        state.state_timeout_tracker.pending_snapshot().is_empty(),
        "an unreadable tail must not arm a timeout from stale snapshot state"
    );

    let (snapshot_sequence, snapshot_bytes) = sim_store
        .load_snapshot(&actor_key)
        .await
        .expect("snapshot remains readable")
        .expect("legacy boundary remains present");
    assert_eq!(snapshot_sequence, 1);
    assert_eq!(
        snapshot_bytes, legacy_snapshot_bytes,
        "an unreadable tail must not authorize a legacy snapshot rewrite"
    );
    assert_eq!(
        sim_store.dump_segments(&actor_key),
        segments_before,
        "failed hydration must not rotate snapshot segment metadata"
    );
    let events = sim_store
        .read_events(&actor_key, 0)
        .await
        .expect("journal becomes readable after the injected failure");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].event_type, "TimeoutFail");
}
