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
async fn legacy_and_default_qualified_ids_share_one_physical_stream() {
    let store = SimEventStore::no_faults(42);
    let legacy = "Order:alias-cross-call";
    let qualified = "default:Order:alias-cross-call";

    store.inject_concurrency_violations(legacy, 1);
    let injected = store
        .append(qualified, 0, &[test_envelope(0, "Created")])
        .await
        .expect_err("fault injection through an alias must hit the same stream");
    assert!(matches!(
        injected,
        PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 0
        }
    ));
    assert_eq!(store.pending_concurrency_violations(qualified), 0);

    assert_eq!(
        store
            .append(legacy, 0, &[test_envelope(0, "Created")])
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .append(qualified, 1, &[test_envelope(0, "Updated")])
            .await
            .unwrap(),
        2
    );

    let legacy_events = store.read_events(legacy, 0).await.unwrap();
    let qualified_events = store.read_events(qualified, 0).await.unwrap();
    assert_eq!(
        serde_json::to_value(&legacy_events).unwrap(),
        serde_json::to_value(&qualified_events).unwrap()
    );
    assert_eq!(legacy_events.len(), 2);
    assert_eq!(store.entity_count(), 1);
    assert_eq!(
        store.list_all_persistence_ids(),
        vec![qualified.to_string()]
    );
    assert_eq!(store.dump_segments(legacy), store.dump_segments(qualified));
    assert_eq!(store.dump_segments(legacy)[0].event_count, 2);

    let conflict = store
        .append(legacy, 1, &[test_envelope(0, "Stale")])
        .await
        .expect_err("CAS through the legacy alias must observe the qualified tail");
    assert!(matches!(
        conflict,
        PersistenceError::ConcurrencyViolation {
            expected: 1,
            actual: 2
        }
    ));

    store
        .save_snapshot(legacy, 2, b"snapshot-v2")
        .await
        .unwrap();
    assert_eq!(
        store.load_snapshot(qualified).await.unwrap(),
        Some((2, b"snapshot-v2".to_vec()))
    );
    assert_eq!(store.snapshot_history_len(qualified), 1);

    store.fail_next_reads(legacy, 1);
    assert!(store.read_events(qualified, 0).await.is_err());
    assert_eq!(store.read_events(qualified, 0).await.unwrap().len(), 2);

    let latest = store
        .read_latest_events(&[legacy.to_string(), qualified.to_string()])
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&latest[0]).unwrap(),
        serde_json::to_value(&latest[1]).unwrap()
    );
    assert_eq!(latest[0].as_ref().map(|event| event.sequence_nr), Some(2));
}

#[tokio::test]
async fn empty_append_does_not_create_a_discoverable_stream() {
    let store = SimEventStore::no_faults(42);
    let sequence = store.append("default:Order:empty", 7, &[]).await.unwrap();

    assert_eq!(sequence, 7);
    assert!(store.list_entity_ids("default").await.unwrap().is_empty());
    assert!(store.dump_journal("default:Order:empty").is_empty());
}

#[tokio::test]
async fn empty_batch_member_does_not_create_journal_segment_or_discovery_entry() {
    let store = SimEventStore::no_faults(42);
    let persistence_id = "default:Order:empty-batch";
    let result = store
        .append_batch(&[PersistenceAppend {
            persistence_id: persistence_id.to_string(),
            expected_sequence: 7,
            events: vec![],
            key_rows: None,
            vector_rows: Vec::new(),
            reconcile_vectors: false,
        }])
        .await
        .unwrap();

    assert_eq!(result[0].sequence_nr, 7);
    assert!(store.dump_journal(persistence_id).is_empty());
    assert!(store.dump_segments(persistence_id).is_empty());
    assert!(store.list_entity_ids("default").await.unwrap().is_empty());
}

#[tokio::test]
async fn batch_append_updates_segment_metadata_for_every_stream() {
    let store = SimEventStore::no_faults(42);
    let first = "default:Order:batch-segment-a";
    let second = "default:Order:batch-segment-b";
    let appends = [first, second].map(|persistence_id| PersistenceAppend {
        persistence_id: persistence_id.to_string(),
        expected_sequence: 0,
        events: vec![test_envelope(0, "Created"), test_envelope(0, "Updated")],
        key_rows: None,
        vector_rows: Vec::new(),
        reconcile_vectors: false,
    });

    store.append_batch(&appends).await.unwrap();

    for persistence_id in [first, second] {
        assert_eq!(
            store.dump_segments(persistence_id),
            vec![SimEventSegment {
                segment_index: 0,
                start_sequence_nr: 1,
                end_sequence_nr: Some(2),
                snapshot_sequence: None,
                event_count: 2,
                sealed: false,
            }]
        );
    }
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

    let bounded = store.read_events_bounded(pid, 0, 1).await.unwrap();
    assert_eq!(bounded.len(), 1);
    assert_eq!(bounded[0].sequence_nr, 1);
    assert!(
        store
            .read_events_bounded(pid, 0, 0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn append_batch_commits_multiple_journals_atomically() {
    let store = SimEventStore::no_faults(42);
    let appends = vec![
        PersistenceAppend {
            persistence_id: "default:Order:ord-a".to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Created")],
            key_rows: None,
            vector_rows: Vec::new(),
            reconcile_vectors: false,
        },
        PersistenceAppend {
            persistence_id: "default:Order:ord-b".to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Created"), test_envelope(0, "Submitted")],
            key_rows: None,
            vector_rows: Vec::new(),
            reconcile_vectors: false,
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
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            },
            PersistenceAppend {
                persistence_id: "default:Order:ord-existing".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Submitted")],
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
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
async fn guarded_append_checks_context_and_target_in_one_commit() {
    let store = SimEventStore::no_faults(43);
    let context_id = "default:Owner:owner-guard";
    let target_id = "default:Document:document-guard";
    store
        .append(context_id, 0, &[test_envelope(0, "Created")])
        .await
        .unwrap();
    let append = PersistenceAppend {
        persistence_id: target_id.to_string(),
        expected_sequence: 0,
        events: vec![test_envelope(0, "FieldsPatched")],
        key_rows: None,
        vector_rows: Vec::new(),
        reconcile_vectors: false,
    };

    let error = store
        .append_batch_guarded(
            std::slice::from_ref(&append),
            &[PersistenceSequenceGuard {
                persistence_id: context_id.to_string(),
                expected_sequence: 0,
            }],
        )
        .await
        .expect_err("stale context guard must abort target append");
    assert!(matches!(
        error,
        PersistenceError::PreconditionFailed {
            expected: 0,
            actual: 1,
            ..
        }
    ));
    assert!(store.dump_journal(target_id).is_empty());

    let result = store
        .append_batch_guarded(
            &[append],
            &[PersistenceSequenceGuard {
                persistence_id: context_id.to_string(),
                expected_sequence: 1,
            }],
        )
        .await
        .expect("current context guard should commit target");
    assert_eq!(result[0].sequence_nr, 1);
    assert_eq!(store.dump_journal(target_id).len(), 1);
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
