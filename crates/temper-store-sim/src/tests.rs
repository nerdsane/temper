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
async fn pre_reconciliation_live_vector_type_remains_discoverable() {
    let store = SimEventStore::no_faults(41);
    store
        .append_with_index_rows(
            "default:Item:item-before-generation",
            0,
            &[test_envelope(0, "Created")],
            &[],
            &[EntityVectorRow {
                decl_name: "embed".to_string(),
                model_tag: "m1".to_string(),
                vector: vec![1.0, 0.0],
            }],
            true,
        )
        .await
        .unwrap();

    assert_eq!(
        store
            .vector_reconciliation_entity_types("default")
            .await
            .unwrap(),
        vec!["Item".to_string()],
        "generation-zero fences must keep remove-all reconciliation discoverable"
    );
}

#[tokio::test]
async fn stale_vector_backfill_does_not_overwrite_newer_live_write() {
    let store = SimEventStore::no_faults(42);
    let generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|embed")
        .await
        .unwrap();
    let persistence_id = "default:Item:item-race";
    let stale_row = EntityVectorRow {
        decl_name: "embed".to_string(),
        model_tag: "model-v1".to_string(),
        vector: vec![1.0, 0.0],
    };

    store
        .append_with_index_rows(
            persistence_id,
            0,
            &[test_envelope(0, "Created")],
            &[],
            std::slice::from_ref(&stale_row),
            true,
        )
        .await
        .unwrap();

    let live_row = EntityVectorRow {
        decl_name: "embed".to_string(),
        model_tag: "model-v1".to_string(),
        vector: vec![0.0, 1.0],
    };
    store
        .append_with_index_rows(
            persistence_id,
            1,
            &[test_envelope(0, "Updated")],
            &[],
            std::slice::from_ref(&live_row),
            true,
        )
        .await
        .unwrap();

    // Model a rebuild that loaded journal sequence 1 before the live sequence-2
    // append committed, then reached the index after that append.
    store
        .backfill_entity_vectors("default", "Item", "item-race", generation, 1, &[stale_row])
        .await
        .unwrap();

    let candidates = store
        .vector_candidates("default", "Item", "embed", "model-v1", 10)
        .await
        .unwrap();
    assert_eq!(
        candidates,
        vec![EntityVectorCandidate {
            entity_id: "item-race".to_string(),
            vector: live_row.vector.clone(),
        }],
        "a stale rebuild observed at sequence 1 must not overwrite the vector co-committed at sequence 2"
    );

    store
        .append_with_index_rows(
            persistence_id,
            2,
            &[test_envelope(0, "Deleted")],
            &[],
            &[],
            true,
        )
        .await
        .unwrap();
    store
        .backfill_entity_vectors("default", "Item", "item-race", generation, 2, &[live_row])
        .await
        .unwrap();
    assert!(
        store
            .vector_candidates("default", "Item", "embed", "model-v1", 10)
            .await
            .unwrap()
            .is_empty(),
        "a stale sequence-2 rebuild must not resurrect vectors purged at sequence 3"
    );

    // Equal-sequence replay is accepted and remains idempotent, including an
    // empty tombstone that has no physical vector row.
    store
        .backfill_entity_vectors("default", "Item", "item-race", generation, 3, &[])
        .await
        .unwrap();
    store
        .backfill_entity_vectors("default", "Item", "item-race", generation, 3, &[])
        .await
        .unwrap();
}

#[tokio::test]
async fn newer_vector_reconciliation_generation_rejects_delayed_older_set() {
    let store = SimEventStore::no_faults(43);
    let persistence_id = "default:Item:item-generation";
    let old_row = EntityVectorRow {
        decl_name: "old-embed".to_string(),
        model_tag: "m1".to_string(),
        vector: vec![1.0, 0.0],
    };
    let new_row = EntityVectorRow {
        decl_name: "new-embed".to_string(),
        model_tag: "m2".to_string(),
        vector: vec![0.0, 1.0],
    };

    let old_generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|old-embed")
        .await
        .unwrap();
    store
        .append_with_index_rows(
            persistence_id,
            0,
            &[test_envelope(0, "Created")],
            &[],
            std::slice::from_ref(&old_row),
            true,
        )
        .await
        .unwrap();

    // The newer declaration set starts and converges from the same journal
    // sequence before delayed work from the older invocation resumes.
    let new_generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|new-embed")
        .await
        .unwrap();
    store
        .backfill_entity_vectors(
            "default",
            "Item",
            "item-generation",
            new_generation,
            1,
            std::slice::from_ref(&new_row),
        )
        .await
        .unwrap();
    store
        .mark_vector_index_backfilled("default", "Item", new_generation, "v2|new-embed")
        .await
        .unwrap();

    let stale_replace = store
        .backfill_entity_vectors(
            "default",
            "Item",
            "item-generation",
            old_generation,
            1,
            std::slice::from_ref(&old_row),
        )
        .await;
    assert!(
        stale_replace.is_err(),
        "an older declaration-set generation must not replace equal-sequence rows"
    );
    let stale_watermark = store
        .mark_vector_index_backfilled("default", "Item", old_generation, "v2|old-embed")
        .await;
    assert!(
        stale_watermark.is_err(),
        "an older declaration-set generation must not overwrite the newer watermark"
    );
    assert!(
        store
            .vector_candidates("default", "Item", "old-embed", "m1", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .vector_candidates("default", "Item", "new-embed", "m2", 10)
            .await
            .unwrap(),
        vec![EntityVectorCandidate {
            entity_id: "item-generation".to_string(),
            vector: new_row.vector,
        }]
    );
    assert_eq!(
        store
            .vector_index_backfilled_types("default")
            .await
            .unwrap(),
        vec![("Item".to_string(), "v2|new-embed".to_string())]
    );
}

#[tokio::test]
async fn beginning_reconciliation_withdraws_the_previous_completion_claim() {
    let store = SimEventStore::no_faults(45);
    let persistence_id = "default:Item:item-signature-race";
    let row_a = EntityVectorRow {
        decl_name: "embed-a".to_string(),
        model_tag: "m1".to_string(),
        vector: vec![1.0, 0.0],
    };
    let row_b = EntityVectorRow {
        decl_name: "embed-b".to_string(),
        model_tag: "m2".to_string(),
        vector: vec![0.0, 1.0],
    };

    let first_a = store
        .begin_vector_index_reconciliation("default", "Item", "v2|a")
        .await
        .unwrap();
    store
        .append_with_index_rows(
            persistence_id,
            0,
            &[test_envelope(0, "Created")],
            &[],
            std::slice::from_ref(&row_a),
            true,
        )
        .await
        .unwrap();
    store
        .mark_vector_index_backfilled("default", "Item", first_a, "v2|a")
        .await
        .unwrap();

    let generation_b = store
        .begin_vector_index_reconciliation("default", "Item", "v2|b")
        .await
        .unwrap();
    assert!(
        store
            .vector_index_backfilled_types("default")
            .await
            .unwrap()
            .is_empty(),
        "beginning B must atomically withdraw A's completion watermark"
    );
    assert_eq!(
        store
            .vector_reconciliation_entity_types("default")
            .await
            .unwrap(),
        vec!["Item".to_string()],
        "the in-progress type must remain discoverable without its watermark"
    );

    // A coordinator that still owns declaration set A now sees no completion
    // claim, allocates a newer generation, and invalidates delayed B work.
    let second_a = store
        .begin_vector_index_reconciliation("default", "Item", "v2|a")
        .await
        .unwrap();
    assert!(second_a > generation_b);
    store
        .backfill_entity_vectors(
            "default",
            "Item",
            "item-signature-race",
            second_a,
            1,
            std::slice::from_ref(&row_a),
        )
        .await
        .unwrap();
    store
        .mark_vector_index_backfilled("default", "Item", second_a, "v2|a")
        .await
        .unwrap();

    assert!(
        store
            .backfill_entity_vectors(
                "default",
                "Item",
                "item-signature-race",
                generation_b,
                1,
                &[row_b],
            )
            .await
            .is_err()
    );
    assert!(
        store
            .mark_vector_index_backfilled("default", "Item", generation_b, "v2|b")
            .await
            .is_err()
    );
    assert_eq!(
        store
            .vector_index_backfilled_types("default")
            .await
            .unwrap(),
        vec![("Item".to_string(), "v2|a".to_string())]
    );
}

#[tokio::test]
async fn composite_batch_vector_fence_rejects_delayed_repair() {
    let store = SimEventStore::no_faults(44);
    let persistence_id = "default:Item:item-composite";
    let generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|embed")
        .await
        .unwrap();
    let stale_row = EntityVectorRow {
        decl_name: "embed".to_string(),
        model_tag: "m1".to_string(),
        vector: vec![1.0, 0.0],
    };
    let live_row = EntityVectorRow {
        decl_name: "embed".to_string(),
        model_tag: "m1".to_string(),
        vector: vec![0.0, 1.0],
    };

    store
        .append_with_index_rows(
            persistence_id,
            0,
            &[test_envelope(0, "Created")],
            &[],
            std::slice::from_ref(&stale_row),
            true,
        )
        .await
        .unwrap();
    store
        .append_batch(&[PersistenceAppend {
            persistence_id: persistence_id.to_string(),
            expected_sequence: 1,
            events: vec![test_envelope(0, "CompositeUpdated")],
            vector_rows: vec![live_row.clone()],
            reconcile_vectors: true,
        }])
        .await
        .unwrap();
    store
        .backfill_entity_vectors(
            "default",
            "Item",
            "item-composite",
            generation,
            1,
            std::slice::from_ref(&stale_row),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .vector_candidates("default", "Item", "embed", "m1", 10)
            .await
            .unwrap()[0]
            .vector,
        live_row.vector.clone()
    );

    store
        .append_batch(&[PersistenceAppend {
            persistence_id: persistence_id.to_string(),
            expected_sequence: 2,
            events: vec![test_envelope(0, "CompositeDeleted")],
            vector_rows: Vec::new(),
            reconcile_vectors: true,
        }])
        .await
        .unwrap();
    store
        .backfill_entity_vectors(
            "default",
            "Item",
            "item-composite",
            generation,
            2,
            std::slice::from_ref(&live_row),
        )
        .await
        .unwrap();
    assert!(
        store
            .vector_candidates("default", "Item", "embed", "m1", 10)
            .await
            .unwrap()
            .is_empty(),
        "the composite delete's sequence-3 fence must reject sequence-2 resurrection"
    );
    assert_eq!(store.dump_journal(persistence_id).len(), 3);
}

#[tokio::test]
async fn append_batch_commits_multiple_journals_atomically() {
    let store = SimEventStore::no_faults(42);
    let appends = vec![
        PersistenceAppend {
            persistence_id: "default:Order:ord-a".to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Created")],
            vector_rows: Vec::new(),
            reconcile_vectors: false,
        },
        PersistenceAppend {
            persistence_id: "default:Order:ord-b".to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Created"), test_envelope(0, "Submitted")],
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
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            },
            PersistenceAppend {
                persistence_id: "default:Order:ord-existing".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Submitted")],
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
