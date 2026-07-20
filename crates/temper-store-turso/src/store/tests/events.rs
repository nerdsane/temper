use super::*;

#[tokio::test]
async fn append_and_read_events_roundtrip() {
    let store = make_store("append-read").await;
    let persistence_id = "tenant-a:Order:ord-1";

    let new_seq = store
        .append(
            persistence_id,
            0,
            &[
                test_envelope("OrderCreated", serde_json::json!({ "id": "ord-1" })),
                test_envelope("OrderApproved", serde_json::json!({ "approved": true })),
            ],
        )
        .await
        .unwrap();

    assert_eq!(new_seq, 2);

    let read = store
        .read_events_with_head(persistence_id, 0)
        .await
        .unwrap();
    assert_eq!(read.journal_head_sequence_nr, 2);
    let events = read.events;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sequence_nr, 1);
    assert_eq!(events[1].sequence_nr, 2);
    assert_eq!(events[0].event_type, "OrderCreated");
    assert_eq!(events[1].event_type, "OrderApproved");

    let empty_tail = store
        .read_events_with_head(persistence_id, 2)
        .await
        .unwrap();
    assert_eq!(empty_tail.journal_head_sequence_nr, 2);
    assert!(empty_tail.events.is_empty());
}

#[tokio::test]
async fn vector_index_write_behind_candidates_and_partitioning() {
    // ADR-0155: Turso maintains entity_vector_index write-behind (event first, index
    // follows). A candidate scan returns the partition's vectors in entity_id order,
    // partitioned by model tag; a raw kNN read never sees another model's vectors.
    let store = make_store("vector-index").await;
    let row = |decl: &str, model: &str, v: Vec<f32>| EntityVectorRow {
        decl_name: decl.to_string(),
        model_tag: model.to_string(),
        vector: v,
    };

    store
        .append_with_index_rows(
            "t:Item:item-b",
            0,
            &[test_envelope("Create", serde_json::json!({}))],
            &[],
            &[row("embed", "m1", vec![0.0, 1.0])],
            true,
        )
        .await
        .unwrap();
    store
        .append_with_index_rows(
            "t:Item:item-a",
            0,
            &[test_envelope("Create", serde_json::json!({}))],
            &[],
            &[row("embed", "m1", vec![1.0, 0.0])],
            true,
        )
        .await
        .unwrap();
    // A different model tag — must not appear in an m1 scan.
    store
        .append_with_index_rows(
            "t:Item:item-c",
            0,
            &[test_envelope("Create", serde_json::json!({}))],
            &[],
            &[row("embed", "m2", vec![1.0, 0.0])],
            true,
        )
        .await
        .unwrap();

    let candidates = store
        .vector_candidates("t", "Item", "embed", "m1", 1000)
        .await
        .unwrap();
    // Two m1 items, in entity_id order (a before b) with their vectors intact.
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].entity_id, "item-a");
    assert_eq!(candidates[0].vector, vec![1.0, 0.0]);
    assert_eq!(candidates[1].entity_id, "item-b");
    assert_eq!(candidates[1].vector, vec![0.0, 1.0]);

    // Upsert: re-writing item-a's vector replaces (no duplicate row).
    store
        .backfill_entity_vectors("t", "Item", "item-a", &[row("embed", "m1", vec![0.5, 0.5])])
        .await
        .unwrap();
    let candidates = store
        .vector_candidates("t", "Item", "embed", "m1", 1000)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].vector, vec![0.5, 0.5]);

    // Watermark roundtrip + resumable id listing.
    store
        .mark_vector_index_backfilled("t", "Item", "embed")
        .await
        .unwrap();
    assert_eq!(
        store.vector_index_backfilled_types("t").await.unwrap(),
        vec![("Item".to_string(), "embed".to_string())]
    );
    let mut ids = store
        .vectored_entity_ids_for_type("t", "Item")
        .await
        .unwrap();
    ids.sort();
    assert_eq!(ids, vec!["item-a", "item-b", "item-c"]);
}

#[tokio::test]
async fn vector_index_reconcile_purges_on_delete_and_empty_rows() {
    // ADR-0155: a delete/clear reconciles to an empty row set, purging the entity's
    // vector rows (the turso-side "remove" cleanup) so it is never ranked again.
    let store = make_store("vector-purge").await;
    let row = |v: Vec<f32>| EntityVectorRow {
        decl_name: "embed".to_string(),
        model_tag: "m1".to_string(),
        vector: v,
    };

    // Write-behind reconcile with a row, then a delete transition (empty rows).
    store
        .append_with_index_rows(
            "t:Item:item-a",
            0,
            &[test_envelope("Create", serde_json::json!({}))],
            &[],
            std::slice::from_ref(&row(vec![1.0, 0.0])),
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .vector_candidates("t", "Item", "embed", "m1", 10)
            .await
            .unwrap()
            .len(),
        1
    );
    // A Deleted transition emits no vector rows but still reconciles (purge).
    store
        .append_with_index_rows(
            "t:Item:item-a",
            1,
            &[test_envelope("Delete", serde_json::json!({}))],
            &[],
            &[],
            true,
        )
        .await
        .unwrap();
    assert!(
        store
            .vector_candidates("t", "Item", "embed", "m1", 10)
            .await
            .unwrap()
            .is_empty(),
        "the deleted entity's vector row must be purged"
    );

    // The explicit backfill purge (empty rows) is idempotent.
    store
        .backfill_entity_vectors("t", "Item", "item-a", &[])
        .await
        .unwrap();
    assert!(
        store
            .vector_candidates("t", "Item", "embed", "m1", 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn append_with_wrong_sequence_fails_with_concurrency_violation() {
    let store = make_store("concurrency").await;
    let persistence_id = "tenant-a:Order:ord-2";

    store
        .append(
            persistence_id,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-2" }),
            )],
        )
        .await
        .unwrap();

    let err = store
        .append(
            persistence_id,
            0,
            &[test_envelope(
                "OrderUpdated",
                serde_json::json!({ "step": 2 }),
            )],
        )
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
async fn append_batch_zero_sequence_detects_existing_stream_by_unique_key() {
    let store = make_store("append-batch-zero-seq-conflict").await;
    let persistence_id = "tenant-a:Order:ord-batch-conflict";

    store
        .append(
            persistence_id,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-batch-conflict" }),
            )],
        )
        .await
        .unwrap();

    let err = store
        .append_batch(&[PersistenceAppend {
            persistence_id: persistence_id.to_string(),
            expected_sequence: 0,
            events: vec![test_envelope(
                "OrderUpdated",
                serde_json::json!({ "step": 2 }),
            )],
        }])
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 1
        }
    ));

    let events = store.read_events(persistence_id, 0).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "OrderCreated");
}

#[tokio::test]
async fn single_event_append_bypasses_process_write_gate() {
    let mut store = make_store("single-append-bypasses-gate").await;
    store.write_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let held_gate = store
        .write_gate
        .clone()
        .acquire_owned()
        .await
        .expect("hold gate");

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store.append(
            "tenant-a:Order:ord-bypass",
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-bypass" }),
            )],
        ),
    )
    .await;
    drop(held_gate);

    let new_seq = result
        .expect("single-event append should not wait for the process write gate")
        .expect("append should succeed");
    assert_eq!(new_seq, 1);
}
