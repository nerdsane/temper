//! Snapshot-boundary and replacement durability regressions.

use super::*;

#[tokio::test]
async fn snapshot_save_records_history_and_rotates_segments() {
    let store = SimEventStore::no_faults(42);
    let pid = "default:Order:segmented";

    store
        .append(
            pid,
            0,
            &[test_envelope(0, "Created"), test_envelope(0, "Updated")],
        )
        .await
        .unwrap();
    store.save_snapshot(pid, 2, b"snapshot-2").await.unwrap();
    store
        .append(pid, 2, &[test_envelope(0, "AfterSnapshot")])
        .await
        .unwrap();

    assert_eq!(store.snapshot_history_len(pid), 1);
    let segments = store.dump_segments(pid);
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
async fn snapshot_replacement_preserves_existing_segment_boundary() {
    let store = SimEventStore::no_faults(42);
    let pid = "default:Order:snapshot-rewrite";

    let missing = store
        .replace_snapshot(pid, 0, b"must-not-exist", b"must-not-create")
        .await
        .unwrap_err();
    assert!(matches!(missing, PersistenceError::Storage(_)));
    assert_eq!(store.load_snapshot(pid).await.unwrap(), None);

    store
        .append(
            pid,
            0,
            &[test_envelope(0, "Created"), test_envelope(0, "Updated")],
        )
        .await
        .unwrap();
    store
        .save_snapshot(pid, 2, b"legacy-snapshot")
        .await
        .unwrap();
    let segments_before = store.dump_segments(pid);

    store
        .replace_snapshot(pid, 2, b"legacy-snapshot", b"upgraded-snapshot")
        .await
        .unwrap();

    assert_eq!(
        store.load_snapshot(pid).await.unwrap(),
        Some((2, b"upgraded-snapshot".to_vec()))
    );
    assert_eq!(store.snapshot_history_len(pid), 1);
    assert_eq!(
        store.snapshot_history_at(pid, 2),
        Some(b"upgraded-snapshot".to_vec())
    );
    assert_eq!(store.dump_segments(pid), segments_before);
}

#[tokio::test]
async fn snapshot_replacement_rejects_a_stale_same_boundary_writer() {
    let store = SimEventStore::no_faults(42);
    let pid = "default:Order:concurrent-snapshot-repair";
    store
        .save_snapshot(pid, 2, b"legacy-snapshot")
        .await
        .expect("seed legacy boundary");

    store
        .replace_snapshot(pid, 2, b"legacy-snapshot", b"first-repair")
        .await
        .expect("first repair claims the legacy boundary");
    let stale_repair = store
        .replace_snapshot(pid, 2, b"legacy-snapshot", b"stale-second-repair")
        .await
        .expect_err("a second writer that loaded the legacy boundary must lose");

    assert!(
        matches!(
            stale_repair,
            PersistenceError::ConcurrencyViolation { .. } | PersistenceError::Storage(_)
        ),
        "unexpected stale-repair error: {stale_repair}"
    );
    assert_eq!(
        store.load_snapshot(pid).await.unwrap(),
        Some((2, b"first-repair".to_vec())),
        "the winning repair must not be overwritten"
    );
}
