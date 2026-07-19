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
    store.persist_spec_declaration("default", "Item", "rev-pre");
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
            Some("rev-pre"),
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
    store.persist_spec_declaration("default", "Item", "rev-1");
    let generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|embed", 1, "rev-1")
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
            Some("rev-1"),
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
            Some("rev-1"),
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
            Some("rev-1"),
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

    store.persist_spec_declaration("default", "Item", "rev-old");
    let old_generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|old-embed", 1, "rev-old")
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
            Some("rev-old"),
        )
        .await
        .unwrap();

    // The newer declaration set starts and converges from the same journal
    // sequence before delayed work from the older invocation resumes.
    store.persist_spec_declaration("default", "Item", "rev-new");
    let new_generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|new-embed", 2, "rev-new")
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
async fn stale_declaration_cannot_reclaim_generation_after_newer_set_completes() {
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

    store.persist_spec_declaration("default", "Item", "rev-a");
    let first_a = store
        .begin_vector_index_reconciliation("default", "Item", "v2|a", 1, "rev-a")
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
            Some("rev-a"),
        )
        .await
        .unwrap();
    store
        .mark_vector_index_backfilled("default", "Item", first_a, "v2|a")
        .await
        .unwrap();

    store.persist_spec_declaration("default", "Item", "rev-b");
    let generation_b = store
        .begin_vector_index_reconciliation("default", "Item", "v2|b", 2, "rev-b")
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

    let stale_live = store
        .append_with_index_rows(
            persistence_id,
            1,
            &[test_envelope(0, "StaleReplicaUpdated")],
            &[],
            std::slice::from_ref(&row_a),
            true,
            Some("rev-a"),
        )
        .await;
    assert!(
        stale_live.is_err(),
        "a stale replica must not advance the journal with rows from declaration A"
    );
    assert_eq!(store.dump_journal(persistence_id).len(), 1);

    store
        .append_with_index_rows(
            persistence_id,
            1,
            &[test_envelope(0, "CurrentReplicaUpdated")],
            &[],
            std::slice::from_ref(&row_b),
            true,
            Some("rev-b"),
        )
        .await
        .unwrap();

    store
        .backfill_entity_vectors(
            "default",
            "Item",
            "item-signature-race",
            generation_b,
            1,
            std::slice::from_ref(&row_b),
        )
        .await
        .unwrap();
    store
        .mark_vector_index_backfilled("default", "Item", generation_b, "v2|b")
        .await
        .unwrap();

    // A stale replica still holding declaration set A must not obtain a later
    // generation after the authoritative B revision has completed.
    let stale_a = store
        .begin_vector_index_reconciliation("default", "Item", "v2|a", 1, "rev-a")
        .await;
    assert!(
        stale_a.is_err(),
        "an older declaration revision must not reclaim authority by arriving last"
    );
    assert_eq!(
        store
            .vector_index_backfilled_types("default")
            .await
            .unwrap(),
        vec![("Item".to_string(), "v2|b".to_string())]
    );
    assert_eq!(
        store
            .vector_candidates("default", "Item", "embed-b", "m2", 10)
            .await
            .unwrap(),
        vec![EntityVectorCandidate {
            entity_id: "item-signature-race".to_string(),
            vector: row_b.vector,
        }]
    );
}

#[tokio::test]
async fn caller_local_revision_cannot_override_durable_declaration_authority() {
    let store = SimEventStore::no_faults(48);
    store.persist_spec_declaration("default", "Item", "rev-a");
    let generation_a = store
        .begin_vector_index_reconciliation("default", "Item", "v2|a", 1, "rev-a")
        .await
        .unwrap();
    store
        .mark_vector_index_backfilled("default", "Item", generation_a, "v2|a")
        .await
        .unwrap();

    store.persist_spec_declaration("default", "Item", "rev-b");
    let generation_b = store
        .begin_vector_index_reconciliation("default", "Item", "v2|b", 1, "rev-b")
        .await
        .unwrap();
    store
        .mark_vector_index_backfilled("default", "Item", generation_b, "v2|b")
        .await
        .unwrap();

    let stale = store
        .begin_vector_index_reconciliation("default", "Item", "v2|a", u64::MAX, "rev-a")
        .await;
    assert!(
        stale.is_err(),
        "even a maximal caller-local revision must not replace persisted B authority"
    );
    assert_eq!(
        store
            .vector_index_backfilled_types("default")
            .await
            .unwrap(),
        vec![("Item".to_string(), "v2|b".to_string())]
    );
}

#[tokio::test]
async fn fresh_reconciliation_ignores_maximal_caller_revision() {
    let store = SimEventStore::no_faults(52);
    let first_generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|a", u64::MAX, "rev-a")
        .await
        .expect("bootstrap fresh declaration authority");
    assert_eq!(first_generation, 1);

    let next_revision = store.persist_spec_declaration("default", "Item", "rev-b");
    assert_eq!(
        next_revision, 2,
        "durable authority must start at revision one"
    );
    assert!(
        store
            .begin_vector_index_reconciliation("default", "Item", "v2|a", 1, "rev-a")
            .await
            .is_err(),
        "the next persisted declaration must fence the fresh generation"
    );
    assert_eq!(
        store
            .begin_vector_index_reconciliation("default", "Item", "v2|b", 1, "rev-b")
            .await
            .expect("begin next declaration"),
        2
    );
}

#[tokio::test]
async fn rejected_single_append_does_not_publish_bootstrapped_authority() {
    let store = SimEventStore::no_faults(49);
    let claimed_key = temper_runtime::persistence::EntityKeyRow {
        key_name: "external-id".to_string(),
        key_hash: "shared-key".to_string(),
    };
    store
        .append_with_index_rows(
            "default:Item:owner",
            0,
            &[test_envelope(0, "Created")],
            std::slice::from_ref(&claimed_key),
            &[],
            false,
            None,
        )
        .await
        .unwrap();

    let rejected = store
        .append_with_index_rows(
            "default:Item:contender",
            0,
            &[test_envelope(0, "Created")],
            std::slice::from_ref(&claimed_key),
            &[],
            true,
            Some("rev-a"),
        )
        .await;
    assert!(rejected.is_err(), "duplicate key must reject the append");
    assert!(store.dump_journal("default:Item:contender").is_empty());
    assert!(
        store
            .vector_reconciliation_entity_types("default")
            .await
            .unwrap()
            .is_empty(),
        "a rejected append must not leak a generation-zero work row"
    );

    let generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|b", 1, "rev-b")
        .await
        .expect("the rejected rev-a bootstrap must not become durable authority");
    assert_eq!(generation, 1);
}

#[tokio::test]
async fn rejected_batch_does_not_publish_bootstrapped_authority() {
    let store = SimEventStore::no_faults(50);
    let rejected = store
        .append_batch(&[
            PersistenceAppend {
                persistence_id: "default:Item:first".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Created")],
                vector_rows: Vec::new(),
                reconcile_vectors: true,
                spec_declaration_fingerprint: Some("rev-a".to_string()),
            },
            PersistenceAppend {
                persistence_id: "malformed".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Created")],
                vector_rows: Vec::new(),
                reconcile_vectors: true,
                spec_declaration_fingerprint: Some("rev-a".to_string()),
            },
        ])
        .await;
    assert!(
        rejected.is_err(),
        "malformed second stream must abort the batch"
    );
    assert!(store.dump_journal("default:Item:first").is_empty());
    assert!(store.dump_journal("malformed").is_empty());
    assert!(
        store
            .vector_reconciliation_entity_types("default")
            .await
            .unwrap()
            .is_empty(),
        "an aborted batch must not leak authority-derived work"
    );

    let generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|b", 1, "rev-b")
        .await
        .expect("the aborted rev-a batch must not become durable authority");
    assert_eq!(generation, 1);
}

#[tokio::test]
async fn deleted_declaration_reconciliation_resumes_after_store_restart() {
    let store = SimEventStore::no_faults(46);
    store.persist_spec_declaration("default", "Item", "rev-a");
    let present_generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|embed", 1, "rev-a")
        .await
        .unwrap();
    store
        .mark_vector_index_backfilled("default", "Item", present_generation, "v2|embed")
        .await
        .unwrap();

    store.persist_spec_declaration("default", "Item", "absent:v1");
    let absent_generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|", 2, "absent:v1")
        .await
        .unwrap();
    assert!(absent_generation > present_generation);
    assert!(
        store
            .vector_index_backfilled_types("default")
            .await
            .unwrap()
            .is_empty(),
        "starting the deletion purge must withdraw the old watermark"
    );

    // A reopened handle retains durable authority while a rebuilt process-local
    // registry restarts its diagnostic revision at one.
    let restarted = store.clone();
    drop(store);
    let resumed_generation = restarted
        .begin_vector_index_reconciliation("default", "Item", "v2|", 1, "absent:v1")
        .await
        .unwrap();
    assert_eq!(resumed_generation, absent_generation);
    restarted
        .mark_vector_index_backfilled("default", "Item", resumed_generation, "v2|")
        .await
        .unwrap();

    restarted.persist_spec_declaration("default", "Item", "rev-a");
    let readded_generation = restarted
        .begin_vector_index_reconciliation("default", "Item", "v2|embed", 3, "rev-a")
        .await
        .unwrap();
    assert!(
        readded_generation > resumed_generation,
        "an identical declaration re-add must remain a newer authority revision"
    );
}

#[tokio::test]
async fn composite_batch_vector_fence_rejects_delayed_repair() {
    let store = SimEventStore::no_faults(44);
    let persistence_id = "default:Item:item-composite";
    store.persist_spec_declaration("default", "Item", "rev-1");
    let generation = store
        .begin_vector_index_reconciliation("default", "Item", "v2|embed", 1, "rev-1")
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
            Some("rev-1"),
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
            spec_declaration_fingerprint: Some("rev-1".to_string()),
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
            spec_declaration_fingerprint: Some("rev-1".to_string()),
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
            spec_declaration_fingerprint: None,
        },
        PersistenceAppend {
            persistence_id: "default:Order:ord-b".to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Created"), test_envelope(0, "Submitted")],
            vector_rows: Vec::new(),
            reconcile_vectors: false,
            spec_declaration_fingerprint: None,
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
                spec_declaration_fingerprint: None,
            },
            PersistenceAppend {
                persistence_id: "default:Order:ord-existing".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Submitted")],
                vector_rows: Vec::new(),
                reconcile_vectors: false,
                spec_declaration_fingerprint: None,
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
async fn append_batch_preflight_reports_exact_sequence_without_consuming_fault() {
    let store = SimEventStore::no_faults(42);
    let existing = "default:Order:ord-batch-existing";
    let new = "default:Order:ord-batch-new";
    store
        .append(
            existing,
            0,
            &[test_envelope(0, "Created"), test_envelope(0, "Submitted")],
        )
        .await
        .unwrap();
    store.inject_concurrency_violations(existing, 1);

    let error = store
        .append_batch(&[
            PersistenceAppend {
                persistence_id: new.to_string(),
                expected_sequence: 0,
                events: vec![test_envelope(0, "Created")],
                vector_rows: Vec::new(),
                reconcile_vectors: false,
                spec_declaration_fingerprint: None,
            },
            PersistenceAppend {
                persistence_id: existing.to_string(),
                expected_sequence: 99,
                events: vec![test_envelope(0, "Duplicate")],
                vector_rows: Vec::new(),
                reconcile_vectors: false,
                spec_declaration_fingerprint: None,
            },
        ])
        .await
        .expect_err("stale batch must fail before consuming its injected fault");
    assert!(matches!(
        error,
        PersistenceError::ConcurrencyViolation {
            expected: 99,
            actual: 2
        }
    ));
    assert_eq!(store.pending_concurrency_violations(existing), 1);
    assert!(store.dump_journal(new).is_empty());
    assert_eq!(store.dump_journal(existing).len(), 2);

    let injected = store
        .append_batch(&[PersistenceAppend {
            persistence_id: existing.to_string(),
            expected_sequence: 2,
            events: vec![test_envelope(0, "Injected")],
            vector_rows: Vec::new(),
            reconcile_vectors: false,
            spec_declaration_fingerprint: None,
        }])
        .await
        .expect_err("the preserved injected fault must reject the next valid batch");
    assert!(matches!(
        injected,
        PersistenceError::ConcurrencyViolation {
            expected: 2,
            actual: 2
        }
    ));
    assert_eq!(store.pending_concurrency_violations(existing), 0);
    assert_eq!(store.dump_journal(existing).len(), 2);
}

#[tokio::test]
async fn probabilistic_append_batch_reports_unchanged_durable_sequence() {
    let store = SimEventStore::new(
        42,
        SimFaultConfig {
            write_failure_prob: 0.0,
            concurrency_violation_prob: 1.0,
            read_truncation_prob: 0.0,
            snapshot_failure_prob: 0.0,
        },
    );
    let persistence_id = "default:Order:probabilistic-batch-conflict";

    let error = store
        .append_batch(&[PersistenceAppend {
            persistence_id: persistence_id.to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(0, "Created")],
            vector_rows: Vec::new(),
            reconcile_vectors: false,
            spec_declaration_fingerprint: None,
        }])
        .await
        .expect_err("probabilistic concurrency fault must reject the batch");
    assert!(matches!(
        error,
        PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 0
        }
    ));
    assert!(store.dump_journal(persistence_id).is_empty());
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
async fn injected_concurrency_violation_reports_durable_sequence() {
    let store = SimEventStore::new(
        42,
        SimFaultConfig {
            write_failure_prob: 0.0,
            concurrency_violation_prob: 1.0,
            read_truncation_prob: 0.0,
            snapshot_failure_prob: 0.0,
        },
    );
    let pid = "default:Order:injected-conflict";

    let error = store
        .append(pid, 0, &[test_envelope(0, "Created")])
        .await
        .expect_err("injected concurrency violation should reject the append");

    match error {
        PersistenceError::ConcurrencyViolation { expected, actual } => assert_eq!(
            (expected, actual),
            (0, 0),
            "the reported authoritative sequence must match the unchanged journal"
        ),
        other => panic!("unexpected injected error: {other}"),
    }
    assert!(store.dump_journal(pid).is_empty());
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
