//! Recovery regression for a terminal tombstone hidden behind a newer stale snapshot.

use super::*;
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};
use temper_runtime::scheduler::sim_now;
use temper_server::entity_actor::types::EntityEvent;

/// A tombstone is terminal durable history. A newer stale snapshot must not let a
/// restarted actor reappear live, even when the domain action is named `Delete`
/// rather than the legacy canonical `Deleted` event.
#[tokio::test]
async fn actor_recovery_replays_older_tombstone_before_newer_live_snapshot() {
    let (_guard, _clock, _ids) = install_deterministic_context(246);
    let sim = SimEventStore::no_faults(246);
    let events = BoxedEventStore::new(sim.clone());
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)));
    let persistence_id = "default:Doc:stale-snapshot";

    let live_system = ActorSystem::new("arn238-tombstone-snapshot-live");
    let live_actor = live_system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "stale-snapshot",
            table.clone(),
            serde_json::json!({}),
            events.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "stale-snapshot",
    );
    assert!(
        action(
            &live_actor,
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/deleted"}),
        )
        .await
        .success
    );
    let mut stale_live_snapshot = state(&live_actor).await.state;
    let live_sequence = stale_live_snapshot.sequence_nr;
    let tombstone_sequence = live_sequence + 1;
    let stale_snapshot_sequence = tombstone_sequence + 1;
    drop(live_actor);
    drop(live_system);

    let deleted_at = sim_now();
    events
        .append(
            persistence_id,
            live_sequence,
            &[PersistenceEnvelope {
                sequence_nr: tombstone_sequence,
                event_type: "Delete".to_string(),
                payload: serde_json::to_value(EntityEvent {
                    action: "Delete".to_string(),
                    from_status: "Ready".to_string(),
                    to_status: "Deleted".to_string(),
                    timestamp: deleted_at,
                    params: serde_json::json!({}),
                    idempotency_key: None,
                })
                .expect("serialize tombstone"),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: deleted_at,
                    actor_id: persistence_id.to_string(),
                },
            }],
        )
        .await
        .expect("append action-named tombstone");

    stale_live_snapshot.sequence_nr = stale_snapshot_sequence;
    stale_live_snapshot.last_snapshot_sequence_nr = stale_snapshot_sequence;
    stale_live_snapshot.events_since_snapshot = 0;
    events
        .save_snapshot(
            persistence_id,
            stale_snapshot_sequence,
            &serde_json::to_vec(&stale_live_snapshot).expect("serialize stale live snapshot"),
        )
        .await
        .expect("persist newer stale live snapshot");

    let restarted_system = ActorSystem::new("arn238-tombstone-snapshot-restart");
    let restarted = restarted_system.spawn(
        EntityActor::with_persistence(
            "Doc",
            "stale-snapshot",
            table,
            serde_json::json!({}),
            events,
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "stale-snapshot",
    );
    let recovered = state(&restarted).await.state;
    assert_eq!(recovered.status, "Deleted");
    assert_eq!(
        recovered.sequence_nr, tombstone_sequence,
        "the terminal journal boundary, not the stale snapshot, is authoritative"
    );
}
