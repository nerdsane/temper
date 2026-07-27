use super::*;
use temper_runtime::persistence::EventMetadata;

fn test_envelope(event_type: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload: serde_json::json!({"test": true}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::nil(),
            causation_id: uuid::Uuid::nil(),
            correlation_id: uuid::Uuid::nil(),
            timestamp: chrono::DateTime::UNIX_EPOCH,
            actor_id: "test".to_string(),
        },
    }
}

#[tokio::test]
async fn snapshot_save_and_load() {
    let store = SimEventStore::no_faults(42);
    let persistence_id = "default:Order:ord-4";

    store
        .save_snapshot(persistence_id, 5, b"state-data")
        .await
        .unwrap();

    let snapshot = store.load_snapshot(persistence_id).await.unwrap();
    assert_eq!(snapshot, Some((5, b"state-data".to_vec())));
    assert!(
        store.dump_segments(persistence_id).is_empty(),
        "snapshot-only streams must not invent event-segment history"
    );
}

#[tokio::test]
async fn snapshot_save_records_history_and_rotates_segments() {
    let store = SimEventStore::no_faults(42);
    let persistence_id = "default:Order:segmented";

    store
        .append(
            persistence_id,
            0,
            &[test_envelope("Created"), test_envelope("Updated")],
        )
        .await
        .unwrap();
    store
        .save_snapshot(persistence_id, 2, b"snapshot-2")
        .await
        .unwrap();
    store
        .append(persistence_id, 2, &[test_envelope("AfterSnapshot")])
        .await
        .unwrap();

    assert_eq!(store.snapshot_history_len(persistence_id), 1);
    let segments = store.dump_segments(persistence_id);
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].segment_index, 0);
    assert_eq!(segments[0].snapshot_sequence, Some(2));
    assert!(segments[0].sealed);
    assert_eq!(segments[1].segment_index, 1);
    assert_eq!(segments[1].start_sequence_nr, 3);
    assert_eq!(segments[1].end_sequence_nr, Some(3));
    assert!(!segments[1].sealed);
}

#[tokio::test]
async fn delayed_snapshot_does_not_regress_sim_recovery_or_segments() {
    let store = SimEventStore::no_faults(42);
    let persistence_id = "default:Order:delayed-snapshot";
    store
        .append(
            persistence_id,
            0,
            &[test_envelope("Created"), test_envelope("Updated")],
        )
        .await
        .unwrap();
    store
        .save_snapshot(persistence_id, 2, b"snapshot-2")
        .await
        .unwrap();
    store
        .append(persistence_id, 2, &[test_envelope("UpdatedAgain")])
        .await
        .unwrap();
    store
        .save_snapshot(persistence_id, 2, b"snapshot-2-delayed")
        .await
        .unwrap();
    assert!(!store.dump_segments(persistence_id)[1].sealed);

    store
        .save_snapshot(persistence_id, 3, b"snapshot-3")
        .await
        .unwrap();
    store
        .save_snapshot(persistence_id, 1, b"snapshot-1-stale")
        .await
        .unwrap();
    assert_eq!(
        store.load_snapshot(persistence_id).await.unwrap(),
        Some((3, b"snapshot-3".to_vec()))
    );
    assert_eq!(
        store.dump_segments(persistence_id),
        vec![
            SimEventSegment {
                segment_index: 0,
                start_sequence_nr: 1,
                end_sequence_nr: Some(2),
                snapshot_sequence: Some(2),
                event_count: 2,
                sealed: true,
            },
            SimEventSegment {
                segment_index: 1,
                start_sequence_nr: 3,
                end_sequence_nr: Some(3),
                snapshot_sequence: Some(3),
                event_count: 1,
                sealed: true,
            },
            SimEventSegment {
                segment_index: 2,
                start_sequence_nr: 4,
                end_sequence_nr: None,
                snapshot_sequence: None,
                event_count: 0,
                sealed: false,
            },
        ]
    );
}

#[tokio::test]
async fn load_snapshot_returns_none_when_empty() {
    let store = SimEventStore::no_faults(42);
    let snapshot = store
        .load_snapshot("default:Order:nonexistent")
        .await
        .unwrap();
    assert_eq!(snapshot, None);
}
