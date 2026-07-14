use super::*;
use temper_runtime::persistence::EventMetadata;

fn test_envelope(seq: u64, event_type: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: seq,
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
async fn append_and_read_roundtrip() {
    let store = SimEventStore::no_faults(42);
    let pid = "default:Order:ord-1";

    let new_seq = store
        .append(pid, 0, &[test_envelope(0, "Created")])
        .await
        .unwrap();
    assert_eq!(new_seq, 1);

    let events = store.read_events(pid, 0).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence_nr, 1);
    assert_eq!(events[0].event_type, "Created");
}

#[tokio::test]
async fn append_multiple_events() {
    let store = SimEventStore::no_faults(42);
    let pid = "default:Order:ord-2";

    let seq = store
        .append(
            pid,
            0,
            &[test_envelope(0, "Created"), test_envelope(0, "Submitted")],
        )
        .await
        .unwrap();
    assert_eq!(seq, 2);

    let events = store.read_events(pid, 0).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sequence_nr, 1);
    assert_eq!(events[1].sequence_nr, 2);
}

#[tokio::test]
async fn append_batch_commits_multiple_journals_atomically() {
    let store = SimEventStore::no_faults(42);
    let appends = vec![
        PersistenceAppend {
            persistence_id: "default:Order:ord-a".to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Created")],
        },
        PersistenceAppend {
            persistence_id: "default:Order:ord-b".to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Created"), test_envelope(0, "Submitted")],
        },
    ];

    let results = store.append_batch(&appends).await.unwrap();

    assert_eq!(
        results,
        vec![
            PersistenceAppendResult {
                persistence_id: "default:Order:ord-a".to_string(),
                sequence_nr: 1,
            },
            PersistenceAppendResult {
                persistence_id: "default:Order:ord-b".to_string(),
                sequence_nr: 2,
            },
        ]
    );
    assert_eq!(store.dump_journal("default:Order:ord-a").len(), 1);
    assert_eq!(store.dump_journal("default:Order:ord-b").len(), 2);
}

#[tokio::test]
async fn append_batch_conflict_leaves_all_journals_untouched() {
    let store = SimEventStore::no_faults(42);
    store
        .append(
            "default:Order:ord-existing",
            0,
            &[test_envelope(0, "Created")],
        )
        .await
        .unwrap();

    let err = store
        .append_batch(&[
            PersistenceAppend {
                persistence_id: "default:Order:ord-new".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Created")],
            },
            PersistenceAppend {
                persistence_id: "default:Order:ord-existing".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Submitted")],
            },
        ])
        .await
        .expect_err("second journal conflict should abort entire batch");

    assert!(
        matches!(err, PersistenceError::ConcurrencyViolation { .. }),
        "unexpected error: {err}"
    );
    assert!(
        store.dump_journal("default:Order:ord-new").is_empty(),
        "first append must not be persisted when a later stream conflicts"
    );
    assert_eq!(
        store.dump_journal("default:Order:ord-existing").len(),
        1,
        "conflicting stream must keep its original journal only"
    );
}

#[tokio::test]
async fn concurrency_violation_on_wrong_sequence() {
    let store = SimEventStore::no_faults(42);
    let pid = "default:Order:ord-3";

    store
        .append(pid, 0, &[test_envelope(0, "Created")])
        .await
        .unwrap();

    let err = store
        .append(pid, 0, &[test_envelope(0, "Duplicate")])
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 1
        }
    ));
}

#[tokio::test]
async fn injected_concurrency_violations_report_the_durable_journal_head() {
    const DETERMINISTIC_SINGLE: &str = "default:Order:deterministic-single";
    const DETERMINISTIC_BATCH: &str = "default:Order:deterministic-batch";
    const PROBABILISTIC_SINGLE: &str = "default:Order:probabilistic-single";
    const PROBABILISTIC_BATCH: &str = "default:Order:probabilistic-batch";

    let store = SimEventStore::no_faults(42);
    for persistence_id in [
        DETERMINISTIC_SINGLE,
        DETERMINISTIC_BATCH,
        PROBABILISTIC_SINGLE,
        PROBABILISTIC_BATCH,
    ] {
        store
            .append(persistence_id, 0, &[test_envelope(0, "Created")])
            .await
            .expect("seed durable journal head");
    }

    store.inject_concurrency_violations(DETERMINISTIC_SINGLE, 1);
    let deterministic_append_error = store
        .append(DETERMINISTIC_SINGLE, 0, &[test_envelope(0, "Duplicate")])
        .await
        .expect_err("deterministic single append conflict");
    assert!(matches!(
        deterministic_append_error,
        PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 1
        }
    ));

    store.inject_concurrency_violations(DETERMINISTIC_BATCH, 1);
    let deterministic_batch_error = store
        .append_batch(&[PersistenceAppend {
            persistence_id: DETERMINISTIC_BATCH.to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Duplicate")],
        }])
        .await
        .expect_err("deterministic batch append conflict");
    assert!(matches!(
        deterministic_batch_error,
        PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 1
        }
    ));

    let faults = SimFaultConfig {
        write_failure_prob: 0.0,
        concurrency_violation_prob: 1.0,
        read_truncation_prob: 0.0,
        snapshot_failure_prob: 0.0,
    };
    store.restore_faults(faults);

    let append_error = store
        .append(PROBABILISTIC_SINGLE, 0, &[test_envelope(0, "Duplicate")])
        .await
        .expect_err("injected single append conflict");
    assert!(matches!(
        append_error,
        PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 1
        }
    ));

    let batch_error = store
        .append_batch(&[PersistenceAppend {
            persistence_id: PROBABILISTIC_BATCH.to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Duplicate")],
        }])
        .await
        .expect_err("injected batch append conflict");
    assert!(matches!(
        batch_error,
        PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 1
        }
    ));

    for persistence_id in [
        DETERMINISTIC_SINGLE,
        DETERMINISTIC_BATCH,
        PROBABILISTIC_SINGLE,
        PROBABILISTIC_BATCH,
    ] {
        assert_eq!(store.dump_journal(persistence_id).len(), 1);
    }
}

#[tokio::test]
async fn snapshot_save_and_load() {
    let store = SimEventStore::no_faults(42);
    let pid = "default:Order:ord-4";

    store.save_snapshot(pid, 5, b"state-data").await.unwrap();

    let snap = store.load_snapshot(pid).await.unwrap();
    assert_eq!(snap, Some((5, b"state-data".to_vec())));
}

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
async fn load_snapshot_returns_none_when_empty() {
    let store = SimEventStore::no_faults(42);
    let snap = store
        .load_snapshot("default:Order:nonexistent")
        .await
        .unwrap();
    assert_eq!(snap, None);
}

#[tokio::test]
async fn list_entity_ids_filters_by_tenant() {
    let store = SimEventStore::no_faults(42);

    store
        .append("alpha:Order:ord-1", 0, &[test_envelope(0, "Created")])
        .await
        .unwrap();
    store
        .append("alpha:Task:task-1", 0, &[test_envelope(0, "Created")])
        .await
        .unwrap();
    store
        .append("beta:Order:ord-9", 0, &[test_envelope(0, "Created")])
        .await
        .unwrap();

    let mut alpha = store.list_entity_ids("alpha").await.unwrap();
    alpha.sort();
    assert_eq!(
        alpha,
        vec![
            ("Order".to_string(), "ord-1".to_string()),
            ("Task".to_string(), "task-1".to_string()),
        ]
    );

    let beta = store.list_entity_ids("beta").await.unwrap();
    assert_eq!(beta, vec![("Order".to_string(), "ord-9".to_string())]);
}

#[tokio::test]
async fn read_events_from_sequence() {
    let store = SimEventStore::no_faults(42);
    let pid = "default:Order:ord-5";

    store
        .append(pid, 0, &[test_envelope(0, "A"), test_envelope(0, "B")])
        .await
        .unwrap();
    store
        .append(pid, 2, &[test_envelope(0, "C")])
        .await
        .unwrap();

    // Read from sequence 1 — should skip event at seq 1
    let events = store.read_events(pid, 1).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sequence_nr, 2);
    assert_eq!(events[1].sequence_nr, 3);
}

#[tokio::test]
async fn deterministic_across_seeds() {
    // Same seed → same behavior (with no faults, behavior is trivially the same)
    for seed in [42, 123, 999] {
        let store = SimEventStore::no_faults(seed);
        let pid = "default:Order:det-1";

        let seq = store
            .append(pid, 0, &[test_envelope(0, "Created")])
            .await
            .unwrap();
        assert_eq!(seq, 1);

        let events = store.read_events(pid, 0).await.unwrap();
        assert_eq!(events.len(), 1);
    }
}

#[tokio::test]
async fn fault_injection_produces_errors() {
    let faults = SimFaultConfig {
        write_failure_prob: 1.0, // always fail
        concurrency_violation_prob: 0.0,
        read_truncation_prob: 0.0,
        snapshot_failure_prob: 0.0,
    };
    let store = SimEventStore::new(42, faults);
    let pid = "default:Order:fault-1";

    let err = store.append(pid, 0, &[test_envelope(0, "Created")]).await;
    assert!(err.is_err());
}
