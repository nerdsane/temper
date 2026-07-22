use super::*;
use temper_runtime::persistence::{
    EventMetadata, STATE_MATERIALIZATION_EVENT_TYPE, STATE_MATERIALIZATION_SCHEMA,
};

mod key_index;

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

fn materialization_envelope(entity_type: &str, entity_id: &str) -> PersistenceEnvelope {
    let mut envelope = test_envelope(0, STATE_MATERIALIZATION_EVENT_TYPE);
    envelope.payload = serde_json::json!({
        "schema": STATE_MATERIALIZATION_SCHEMA,
        "state": {
            "entity_type": entity_type,
            "entity_id": entity_id,
            "status": "Ready",
            "item_count": 0,
            "counters": {},
            "booleans": {},
            "lists": {},
            "fields": {"Id": entity_id, "Status": "Ready"},
            "events": [],
            "total_event_count": 0,
            "events_since_snapshot": 0,
            "last_snapshot_sequence_nr": 0,
            "sequence_nr": 0,
            "processed_idempotency_keys": {},
        }
    });
    envelope
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
            key_rows: Vec::new(),
            reconcile_keys: false,
            key_set_signature: None,
            snapshot_source: Default::default(),
            batch_idempotency: None,
        },
        PersistenceAppend {
            persistence_id: "default:Order:ord-b".to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Created"), test_envelope(0, "Submitted")],
            key_rows: Vec::new(),
            reconcile_keys: false,
            key_set_signature: None,
            snapshot_source: Default::default(),
            batch_idempotency: None,
        },
    ];

    let results = store.append_batch(&appends).await.unwrap();

    assert_eq!(
        results,
        vec![
            PersistenceAppendResult {
                persistence_id: "default:Order:ord-a".to_string(),
                sequence_nr: 1,
                batch_already_applied: false,
            },
            PersistenceAppendResult {
                persistence_id: "default:Order:ord-b".to_string(),
                sequence_nr: 2,
                batch_already_applied: false,
            },
        ]
    );
    assert_eq!(store.dump_journal("default:Order:ord-a").len(), 1);
    assert_eq!(store.dump_journal("default:Order:ord-b").len(), 2);
}

#[tokio::test]
async fn append_batch_replays_a_content_bound_claim_without_rescanning_journals() {
    let store = SimEventStore::no_faults(96);
    let claim = temper_runtime::persistence::PersistenceBatchIdempotency {
        persistence_id: "default:Repository:repo-1".to_string(),
        idempotency_key: "push-1".to_string(),
        intent_hash: "intent-a".to_string(),
    };
    let appends = vec![
        PersistenceAppend {
            persistence_id: "default:Repository:repo-1".to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "CompositeEvent")],
            key_rows: Vec::new(),
            reconcile_keys: false,
            key_set_signature: None,
            snapshot_source: Default::default(),
            batch_idempotency: Some(claim.clone()),
        },
        PersistenceAppend {
            persistence_id: "default:Commit:commit-1".to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Create")],
            key_rows: Vec::new(),
            reconcile_keys: false,
            key_set_signature: None,
            snapshot_source: Default::default(),
            batch_idempotency: None,
        },
    ];
    store.append_batch(&appends).await.expect("first batch");

    let retry = store
        .append_batch(&appends)
        .await
        .expect("committed retry must be a no-op before stale sequence checks");
    assert!(retry.iter().all(|result| result.batch_already_applied));
    assert_eq!(store.dump_journal("default:Repository:repo-1").len(), 1);
    assert_eq!(store.dump_journal("default:Commit:commit-1").len(), 1);

    let mut conflicting = appends;
    conflicting[0]
        .batch_idempotency
        .as_mut()
        .expect("claim")
        .intent_hash = "intent-b".to_string();
    let error = store
        .append_batch(&conflicting)
        .await
        .expect_err("same idempotency key with different work must fail");
    assert!(error.to_string().contains("different intent"));
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
                key_rows: Vec::new(),
                reconcile_keys: false,
                key_set_signature: None,
                snapshot_source: Default::default(),
                batch_idempotency: None,
            },
            PersistenceAppend {
                persistence_id: "default:Order:ord-existing".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Submitted")],
                key_rows: Vec::new(),
                reconcile_keys: false,
                key_set_signature: None,
                snapshot_source: Default::default(),
                batch_idempotency: None,
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
async fn delayed_snapshot_splits_durable_tail_and_equal_rewrite_preserves_topology() {
    let store = SimEventStore::no_faults(282);
    let pid = "default:Order:delayed-segment-snapshot";
    let events = (1..=10)
        .map(|sequence| test_envelope(sequence, "Updated"))
        .collect::<Vec<_>>();

    store.append(pid, 0, &events).await.unwrap();
    store.save_snapshot(pid, 5, b"snapshot-a").await.unwrap();

    let expected = vec![
        SimEventSegment {
            segment_index: 0,
            start_sequence_nr: 1,
            end_sequence_nr: Some(5),
            snapshot_sequence: Some(5),
            event_count: 5,
            sealed: true,
        },
        SimEventSegment {
            segment_index: 1,
            start_sequence_nr: 6,
            end_sequence_nr: Some(10),
            snapshot_sequence: None,
            event_count: 5,
            sealed: false,
        },
    ];
    assert_eq!(
        store.dump_segments(pid),
        expected,
        "a delayed snapshot must split and retain the already-durable tail"
    );

    let topology_before_rewrite = store.dump_segments(pid);
    store.save_snapshot(pid, 5, b"snapshot-b").await.unwrap();
    assert_eq!(
        store.dump_segments(pid),
        topology_before_rewrite,
        "same-sequence source replacement must not rotate segment topology"
    );
    assert_eq!(store.snapshot_history_len(pid), 1);
    assert_eq!(
        store.load_snapshot(pid).await.unwrap(),
        Some((5, b"snapshot-b".to_vec()))
    );
}

#[tokio::test]
async fn snapshot_only_and_batch_appends_keep_journal_segments_contiguous() {
    let store = SimEventStore::no_faults(283);
    let snapshot_only = "default:Order:snapshot-only-first-journal";
    let snapshot = b"snapshot-only".to_vec();
    store
        .save_snapshot(snapshot_only, 5, &snapshot)
        .await
        .unwrap();
    assert!(
        store.dump_segments(snapshot_only).is_empty(),
        "a snapshot-only generation must not invent journal segments"
    );
    store
        .append_with_index_rows(
            snapshot_only,
            0,
            &[test_envelope(0, "FirstJournalEvent")],
            &[],
            &[],
            IndexReconciliation {
                snapshot_source: SnapshotSourceFence::Exact {
                    sequence_nr: 5,
                    state: snapshot,
                },
                ..IndexReconciliation::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.dump_segments(snapshot_only),
        vec![SimEventSegment {
            segment_index: 0,
            start_sequence_nr: 1,
            end_sequence_nr: Some(1),
            snapshot_sequence: None,
            event_count: 1,
            sealed: false,
        }]
    );

    let batched = "default:Order:batch-after-snapshot";
    store
        .append(batched, 0, &[test_envelope(0, "Created")])
        .await
        .unwrap();
    store
        .save_snapshot(batched, 1, b"snapshot-1")
        .await
        .unwrap();
    store
        .append_batch(&[PersistenceAppend {
            persistence_id: batched.to_string(),
            expected_sequence: 1,
            events: vec![test_envelope(0, "Batched")],
            key_rows: Vec::new(),
            reconcile_keys: false,
            key_set_signature: None,
            snapshot_source: SnapshotSourceFence::Exact {
                sequence_nr: 1,
                state: b"snapshot-1".to_vec(),
            },
            batch_idempotency: None,
        }])
        .await
        .unwrap();
    assert_eq!(
        store.dump_segments(batched),
        vec![
            SimEventSegment {
                segment_index: 0,
                start_sequence_nr: 1,
                end_sequence_nr: Some(1),
                snapshot_sequence: Some(1),
                event_count: 1,
                sealed: true,
            },
            SimEventSegment {
                segment_index: 1,
                start_sequence_nr: 2,
                end_sequence_nr: Some(2),
                snapshot_sequence: None,
                event_count: 1,
                sealed: false,
            },
        ]
    );

    let snapshot_ahead = "default:Order:snapshot-ahead-of-journal";
    store
        .append(
            snapshot_ahead,
            0,
            &[test_envelope(0, "Created"), test_envelope(0, "Updated")],
        )
        .await
        .unwrap();
    let ahead_snapshot = b"snapshot-5".to_vec();
    store
        .save_snapshot(snapshot_ahead, 5, &ahead_snapshot)
        .await
        .unwrap();
    assert_eq!(
        store.dump_segments(snapshot_ahead),
        vec![SimEventSegment {
            segment_index: 0,
            start_sequence_nr: 1,
            end_sequence_nr: Some(2),
            snapshot_sequence: None,
            event_count: 2,
            sealed: false,
        }],
        "snapshot sequence 5 must not rotate topology beyond journal HWM 2"
    );
    store
        .inner
        .lock()
        .expect("SimEventStore lock poisoned")
        .event_segments
        .entry(snapshot_ahead.to_string())
        .or_default()
        .push(SimEventSegment {
            segment_index: 1,
            start_sequence_nr: 6,
            end_sequence_nr: None,
            snapshot_sequence: None,
            event_count: 0,
            sealed: false,
        });
    store
        .append_with_index_rows(
            snapshot_ahead,
            2,
            &[test_envelope(0, "AfterMigrationSnapshot")],
            &[],
            &[],
            IndexReconciliation {
                snapshot_source: SnapshotSourceFence::Exact {
                    sequence_nr: 5,
                    state: ahead_snapshot,
                },
                ..IndexReconciliation::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store.dump_segments(snapshot_ahead),
        vec![SimEventSegment {
            segment_index: 0,
            start_sequence_nr: 1,
            end_sequence_nr: Some(3),
            snapshot_sequence: None,
            event_count: 3,
            sealed: false,
        }],
        "append must rebuild a legacy future-start segment from journal HWM 2"
    );
}

#[tokio::test]
async fn materialized_journal_permanently_retires_unchecked_snapshot_generation() {
    let store = SimEventStore::no_faults(284);
    let persistence_id = "default:Order:materialized-catch-up";
    let migration_snapshot = b"migration-snapshot".to_vec();
    store
        .save_snapshot(persistence_id, 5, &migration_snapshot)
        .await
        .unwrap();
    store
        .append_with_index_rows(
            persistence_id,
            0,
            &[
                materialization_envelope("Order", "materialized-catch-up"),
                test_envelope(0, "Changed"),
            ],
            &[],
            &[],
            IndexReconciliation {
                snapshot_source: SnapshotSourceFence::Exact {
                    sequence_nr: 5,
                    state: migration_snapshot,
                },
                ..IndexReconciliation::default()
            },
        )
        .await
        .unwrap();
    store
        .append(
            persistence_id,
            2,
            &[
                test_envelope(0, "Changed"),
                test_envelope(0, "Changed"),
                test_envelope(0, "Changed"),
                test_envelope(0, "Changed"),
            ],
        )
        .await
        .unwrap();
    let topology_before = store.dump_segments(persistence_id);
    let history_before = store.snapshot_history_len(persistence_id);

    store
        .save_snapshot(persistence_id, 5, b"delayed-unchecked-generation")
        .await
        .unwrap();

    assert_eq!(store.load_snapshot(persistence_id).await.unwrap(), None);
    assert_eq!(store.snapshot_history_len(persistence_id), history_before);
    assert_eq!(store.dump_segments(persistence_id), topology_before);
    assert_eq!(store.dump_journal(persistence_id).len(), 6);

    store
        .save_snapshot_if_source(
            persistence_id,
            6,
            b"checked-journal-snapshot",
            &SnapshotSourceFence::Absent,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store.load_snapshot(persistence_id).await.unwrap(),
        Some((6, b"checked-journal-snapshot".to_vec())),
        "a checked writer may establish the current journal generation's snapshot"
    );
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
async fn read_events_page_applies_cursor_boundary_and_limit() {
    let store = SimEventStore::no_faults(43);
    let persistence_id = "default:Order:ord-page";
    store
        .append(
            persistence_id,
            0,
            &[
                test_envelope(0, "A"),
                test_envelope(0, "B"),
                test_envelope(0, "C"),
                test_envelope(0, "D"),
                test_envelope(0, "E"),
            ],
        )
        .await
        .unwrap();

    let page = store
        .read_events_page(persistence_id, 1, 4, 2)
        .await
        .unwrap();
    assert_eq!(
        page.iter()
            .map(|event| event.sequence_nr)
            .collect::<Vec<_>>(),
        vec![2, 3]
    );
    let final_page = store
        .read_events_page(persistence_id, 3, 4, 8)
        .await
        .unwrap();
    assert_eq!(final_page.len(), 1);
    assert_eq!(final_page[0].sequence_nr, 4);
}

#[tokio::test]
async fn terminal_tombstone_lookup_is_not_fault_truncated() {
    let store = SimEventStore::new(
        43,
        SimFaultConfig {
            write_failure_prob: 0.0,
            concurrency_violation_prob: 0.0,
            read_truncation_prob: 1.0,
            snapshot_failure_prob: 0.0,
        },
    );
    let pid = "default:Order:tombstone-fault";
    let mut deleted = test_envelope(0, "Delete");
    deleted.payload = serde_json::json!({
        "action": "Delete",
        "from_status": "Draft",
        "to_status": "Deleted"
    });
    store
        .append(pid, 0, &[test_envelope(0, "Create"), deleted])
        .await
        .unwrap();

    assert_eq!(store.read_events(pid, 0).await.unwrap().len(), 1);
    assert_eq!(
        store.terminal_tombstone_sequence(pid).await.unwrap(),
        Some(2)
    );
}

#[tokio::test]
async fn snapshot_only_writer_invalidates_inflight_key_coverage() {
    let store = SimEventStore::no_faults(44);
    let signature = "v4:path";
    let revision = store
        .begin_key_index_backfill("default", "Doc", signature)
        .await
        .unwrap();

    store
        .save_snapshot("default:Doc:snapshot-only", 1, b"snapshot-only")
        .await
        .unwrap();

    assert!(
        !store
            .mark_key_index_backfilled_if_revision("default", "Doc", signature, revision)
            .await
            .unwrap(),
        "an entity created after enumeration must reject stale coverage publication"
    );
}

#[tokio::test]
async fn failed_snapshot_write_preserves_key_contract_and_coverage() {
    let store = SimEventStore::new(
        45,
        SimFaultConfig {
            write_failure_prob: 0.0,
            concurrency_violation_prob: 0.0,
            read_truncation_prob: 0.0,
            snapshot_failure_prob: 1.0,
        },
    );
    let original_signature = "v4:path";
    let original_revision = store
        .begin_key_index_backfill("default", "Doc", original_signature)
        .await
        .unwrap();
    store
        .mark_key_index_backfilled("default", "Doc", original_signature)
        .await
        .unwrap();

    let result = store
        .save_snapshot_if_source(
            "default:Doc:failed-snapshot",
            1,
            b"snapshot",
            &SnapshotSourceFence::Unchecked,
            Some("v4:slug"),
        )
        .await;

    assert!(matches!(result, Err(PersistenceError::Storage(_))));
    assert_eq!(
        store
            .key_index_reconciliation_revision("default", "Doc")
            .await
            .unwrap(),
        original_revision,
        "a failed snapshot must not advance the key-contract revision"
    );
    assert_eq!(
        store.key_index_backfilled_types("default").await.unwrap(),
        vec![("Doc".to_string(), original_signature.to_string())],
        "a failed snapshot must not invalidate proven coverage"
    );
    assert_eq!(
        store
            .load_snapshot("default:Doc:failed-snapshot")
            .await
            .unwrap(),
        None
    );
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
