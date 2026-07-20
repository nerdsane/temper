use super::*;
use temper_runtime::persistence::{EntityKeyRow, EventMetadata, KeyIndexBackfillFence};

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
            key_rows: Vec::new(),
            reconcile_keys: false,
            key_set_signature: None,
        },
        PersistenceAppend {
            persistence_id: "default:Order:ord-b".to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Created"), test_envelope(0, "Submitted")],
            key_rows: Vec::new(),
            reconcile_keys: false,
            key_set_signature: None,
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
                key_rows: Vec::new(),
                reconcile_keys: false,
                key_set_signature: None,
            },
            PersistenceAppend {
                persistence_id: "default:Order:ord-existing".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Submitted")],
                key_rows: Vec::new(),
                reconcile_keys: false,
                key_set_signature: None,
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
async fn append_batch_reconciles_keys_and_rolls_back_conflicting_claims() {
    let store = SimEventStore::no_faults(42);
    let owner_pid = "default:Doc:doc-owner";
    let claimant_pid = "default:Doc:doc-claimant";
    let claimed_key = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "shared-path".to_string(),
    };

    store
        .append_with_keys(
            owner_pid,
            0,
            &[test_envelope(0, "Created")],
            std::slice::from_ref(&claimed_key),
        )
        .await
        .unwrap();

    store
        .append_batch(&[
            PersistenceAppend {
                persistence_id: owner_pid.to_string(),
                expected_sequence: 1,
                events: vec![test_envelope(0, "Deleted")],
                key_rows: Vec::new(),
                reconcile_keys: true,
                key_set_signature: Some("v3:path".to_string()),
            },
            PersistenceAppend {
                persistence_id: claimant_pid.to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Created")],
                key_rows: vec![claimed_key.clone()],
                reconcile_keys: true,
                key_set_signature: Some("v3:path".to_string()),
            },
        ])
        .await
        .expect("one atomic batch may release and reclaim the same key");
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", &claimed_key.key_hash)
            .await
            .unwrap(),
        Some("doc-claimant".to_string())
    );
    assert_eq!(store.dump_journal(owner_pid).len(), 2);
    assert_eq!(store.dump_journal(claimant_pid).len(), 1);

    let unrelated_key = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "unrelated-path".to_string(),
    };
    let rejected = store
        .append_batch(&[
            PersistenceAppend {
                persistence_id: "default:Doc:doc-unrelated".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Created")],
                key_rows: vec![unrelated_key.clone()],
                reconcile_keys: true,
                key_set_signature: Some("v3:path".to_string()),
            },
            PersistenceAppend {
                persistence_id: "default:Doc:doc-conflict".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Created")],
                key_rows: vec![claimed_key.clone()],
                reconcile_keys: true,
                key_set_signature: Some("v3:path".to_string()),
            },
        ])
        .await;
    assert!(
        rejected.is_err(),
        "an occupied final key must reject the batch"
    );
    assert!(
        store.dump_journal("default:Doc:doc-unrelated").is_empty(),
        "a later key conflict must roll back an earlier stream"
    );
    assert!(store.dump_journal("default:Doc:doc-conflict").is_empty());
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", &unrelated_key.key_hash)
            .await
            .unwrap(),
        None,
        "the rejected batch must not publish any key rows"
    );
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", &claimed_key.key_hash)
            .await
            .unwrap(),
        Some("doc-claimant".to_string()),
        "the prior owner must survive a rejected batch"
    );
}

#[tokio::test]
async fn key_reconciliation_includes_snapshot_and_key_only_owners() {
    let store = SimEventStore::no_faults(42);
    let orphan = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "orphan-path".to_string(),
    };
    let repair_signature = "v3:path";
    let repair_revision = store
        .begin_key_index_backfill("default", "Doc", repair_signature)
        .await
        .expect("begin orphan repair contract");
    store
        .backfill_entity_keys(
            "default",
            "Doc",
            "orphan-owner",
            0,
            KeyIndexBackfillFence {
                key_set_signature: repair_signature,
                contract_revision: repair_revision,
                expected_journal_sequence: 0,
                expected_entity_live: false,
            },
            &[orphan],
        )
        .await
        .expect("seed key-only orphan");
    store
        .save_snapshot("default:Doc:snapshot-only", 5, b"snapshot-only")
        .await
        .expect("seed snapshot-only owner");
    let repair_revision = store
        .begin_key_index_backfill("default", "Doc", repair_signature)
        .await
        .expect("refresh repair epoch after snapshot-only writer");

    assert_eq!(
        store
            .list_entity_ids_by_type("default", "Doc")
            .await
            .expect("live durable enumeration"),
        vec!["snapshot-only".to_string()],
        "snapshot-only state is live while a key-only orphan is not"
    );
    assert_eq!(
        store
            .list_entity_ids_for_key_reconciliation("default", "Doc")
            .await
            .expect("repair enumeration"),
        vec!["orphan-owner".to_string(), "snapshot-only".to_string()],
        "exact repair must see snapshot and derived owners without journals"
    );

    let snapshot_key = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "snapshot-path".to_string(),
    };
    let stale = store
        .backfill_entity_keys(
            "default",
            "Doc",
            "snapshot-only",
            0,
            KeyIndexBackfillFence {
                key_set_signature: repair_signature,
                contract_revision: repair_revision,
                expected_journal_sequence: 0,
                expected_entity_live: true,
            },
            std::slice::from_ref(&snapshot_key),
        )
        .await;
    assert!(matches!(
        stale,
        Err(PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 5
        })
    ));
    store
        .backfill_entity_keys(
            "default",
            "Doc",
            "snapshot-only",
            5,
            KeyIndexBackfillFence {
                key_set_signature: repair_signature,
                contract_revision: repair_revision,
                expected_journal_sequence: 0,
                expected_entity_live: true,
            },
            std::slice::from_ref(&snapshot_key),
        )
        .await
        .expect("repair snapshot-only owner at durable sequence");
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", "snapshot-path")
            .await
            .expect("snapshot-only lookup"),
        Some("snapshot-only".to_string())
    );
}

#[tokio::test]
async fn journal_source_fence_rejects_equal_sequence_snapshot_repair() {
    let store = SimEventStore::no_faults(42);
    let persistence_id = "default:Doc:source-aba";
    let repair_signature = "v4:path";
    let snapshot_key = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "snapshot-path".to_string(),
    };
    let journal_key = EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "journal-path".to_string(),
    };

    store
        .save_snapshot(persistence_id, 1, b"snapshot-only")
        .await
        .expect("seed snapshot-only generation");
    let repair_revision = store
        .begin_key_index_backfill("default", "Doc", repair_signature)
        .await
        .expect("begin snapshot-derived repair");

    store
        .append_with_index_rows(
            persistence_id,
            0,
            &[test_envelope(0, "Create")],
            std::slice::from_ref(&journal_key),
            &[],
            IndexReconciliation {
                keys: true,
                key_set_signature: Some(repair_signature.to_string()),
                vectors: false,
            },
        )
        .await
        .expect("replace snapshot-only source with equal-sequence journal state");

    let stale = store
        .backfill_entity_keys(
            "default",
            "Doc",
            "source-aba",
            1,
            KeyIndexBackfillFence {
                key_set_signature: repair_signature,
                contract_revision: repair_revision,
                expected_journal_sequence: 0,
                expected_entity_live: true,
            },
            std::slice::from_ref(&snapshot_key),
        )
        .await;
    assert!(matches!(
        stale,
        Err(PersistenceError::JournalBoundaryChanged {
            expected: 0,
            actual: 1,
        })
    ));
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", &journal_key.key_hash)
            .await
            .expect("lookup current journal ownership"),
        Some("source-aba".to_string())
    );
    assert_eq!(
        store
            .lookup_by_key("default", "Doc", "path", &snapshot_key.key_hash)
            .await
            .expect("lookup rejected snapshot ownership"),
        None
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
