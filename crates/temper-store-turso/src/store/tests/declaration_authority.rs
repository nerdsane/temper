use super::*;

#[tokio::test]
async fn durable_spec_revision_rejects_stale_replica_and_allows_later_readd() {
    let store = make_store("vector-durable-spec-revision").await;
    let csdl = "<Schema Namespace=\"Temper.Tests\" />";
    let ioa_a = "[automaton]\nname = \"ItemA\"\n";
    let ioa_b = "[automaton]\nname = \"ItemB\"\n";
    let fingerprint_a = crate::spec_content_hash(ioa_a);
    let fingerprint_b = crate::spec_content_hash(ioa_b);

    store
        .upsert_spec("t", "Item", ioa_a, csdl, &fingerprint_a)
        .await
        .unwrap();
    store.commit_specs("t").await.unwrap();
    let generation_a = store
        .begin_vector_index_reconciliation("t", "Item", "v2|a", 1, &fingerprint_a)
        .await
        .unwrap();
    store
        .mark_vector_index_backfilled("t", "Item", generation_a, "v2|a")
        .await
        .unwrap();

    store
        .upsert_spec("t", "Item", ioa_b, csdl, &fingerprint_b)
        .await
        .unwrap();
    store.commit_specs("t").await.unwrap();
    let generation_b = store
        .begin_vector_index_reconciliation("t", "Item", "v2|b", 1, &fingerprint_b)
        .await
        .unwrap();
    store
        .mark_vector_index_backfilled("t", "Item", generation_b, "v2|b")
        .await
        .unwrap();

    assert!(
        store
            .begin_vector_index_reconciliation("t", "Item", "v2|a", 99, &fingerprint_a)
            .await
            .is_err(),
        "a stale replica fingerprint must be rejected even with a larger caller revision"
    );
    assert_eq!(
        store.vector_index_backfilled_types("t").await.unwrap(),
        vec![("Item".to_string(), "v2|b".to_string())]
    );

    store
        .upsert_spec("t", "Item", ioa_a, csdl, &fingerprint_a)
        .await
        .unwrap();
    store.commit_specs("t").await.unwrap();
    let readded_a = store
        .begin_vector_index_reconciliation("t", "Item", "v2|a", 1, &fingerprint_a)
        .await
        .unwrap();
    assert!(
        readded_a > generation_b,
        "a durable A re-add is a new revision"
    );
}

#[tokio::test]
async fn fresh_store_atomically_bootstraps_first_fingerprinted_declaration() {
    let store = make_store("vector-fresh-authority-bootstrap").await;
    let fingerprint_a = crate::spec_content_hash("fresh declaration A");
    let fingerprint_b = crate::spec_content_hash("fresh declaration B");

    let generation = store
        .begin_vector_index_reconciliation("t", "Item", "v2|embed-a", 1, &fingerprint_a)
        .await
        .expect("first in-memory declaration should establish empty-store authority");
    store
        .append_with_index_rows(
            "t:Item:item-fresh",
            0,
            &[test_envelope("Created", serde_json::json!({}))],
            &[],
            &[EntityVectorRow {
                decl_name: "embed-a".to_string(),
                model_tag: "m1".to_string(),
                vector: vec![1.0, 0.0],
            }],
            true,
            Some(&fingerprint_a),
        )
        .await
        .expect("the authoritative fresh declaration should write");

    assert!(
        store
            .begin_vector_index_reconciliation("t", "Item", "v2|embed-b", 99, &fingerprint_b)
            .await
            .is_err(),
        "a different process-local declaration must not replace first-writer authority"
    );
    assert_eq!(
        store
            .begin_vector_index_reconciliation("t", "Item", "v2|embed-a", 1, &fingerprint_a)
            .await
            .unwrap(),
        generation
    );
    assert_eq!(
        store
            .read_events("t:Item:item-fresh", 0)
            .await
            .unwrap()
            .len(),
        1
    );

    store
        .mark_vector_index_backfilled("t", "Item", generation, "v2|embed-a")
        .await
        .unwrap();
    store
        .delete_spec("t", "Item")
        .await
        .expect("delete must tombstone authority even without a specs row");
    assert!(
        store
            .begin_vector_index_reconciliation("t", "Item", "v2|embed-a", 100, &fingerprint_a)
            .await
            .is_err(),
        "a compatibility authority tombstone must reject the formerly authoritative writer"
    );
    assert!(
        store
            .begin_vector_index_reconciliation("t", "Item", "v2|", 1, "absent:v1")
            .await
            .unwrap()
            > generation
    );
}

#[tokio::test]
async fn stale_vector_writer_cannot_advance_journal_or_replace_reconciled_rows() {
    let store = make_store("vector-stale-writer-fingerprint").await;
    let csdl = "<Schema Namespace=\"Temper.Tests\" />";
    let ioa_a = "[automaton]\nname = \"Item\"\n# declaration-a\n";
    let ioa_b = "[automaton]\nname = \"Item\"\n# declaration-b\n";
    let fingerprint_a = crate::spec_content_hash(ioa_a);
    let fingerprint_b = crate::spec_content_hash(ioa_b);
    let row_a = EntityVectorRow {
        decl_name: "embed-a".to_string(),
        model_tag: "model-a".to_string(),
        vector: vec![1.0, 0.0],
    };
    let row_b = EntityVectorRow {
        decl_name: "embed-b".to_string(),
        model_tag: "model-b".to_string(),
        vector: vec![0.0, 1.0],
    };
    let persistence_id = "t:Item:item-stale-writer";

    store
        .upsert_spec("t", "Item", ioa_a, csdl, &fingerprint_a)
        .await
        .unwrap();
    store.commit_specs("t").await.unwrap();
    let generation_a = store
        .begin_vector_index_reconciliation("t", "Item", "v2|embed-a", 1, &fingerprint_a)
        .await
        .unwrap();
    let missing_fingerprint_id = "t:Item:item-missing-fingerprint";
    let missing_fingerprint_error = store
        .append_with_index_rows(
            missing_fingerprint_id,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
            &[],
            std::slice::from_ref(&row_a),
            true,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            &missing_fingerprint_error,
            PersistenceError::Storage(message)
                if message.contains("requires a spec declaration fingerprint")
        ),
        "unexpected missing-fingerprint error: {missing_fingerprint_error:?}"
    );
    assert!(
        store
            .read_events(missing_fingerprint_id, 0)
            .await
            .unwrap()
            .is_empty()
    );
    store
        .append_with_index_rows(
            persistence_id,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
            &[],
            std::slice::from_ref(&row_a),
            true,
            Some(&fingerprint_a),
        )
        .await
        .unwrap();

    store
        .upsert_spec("t", "Item", ioa_b, csdl, &fingerprint_b)
        .await
        .unwrap();
    store.commit_specs("t").await.unwrap();
    let generation_b = store
        .begin_vector_index_reconciliation("t", "Item", "v2|embed-b", 2, &fingerprint_b)
        .await
        .unwrap();
    assert!(generation_b > generation_a);
    store
        .backfill_entity_vectors(
            "t",
            "Item",
            "item-stale-writer",
            generation_b,
            1,
            std::slice::from_ref(&row_b),
        )
        .await
        .unwrap();
    store
        .mark_vector_index_backfilled("t", "Item", generation_b, "v2|embed-b")
        .await
        .unwrap();

    let fingerprinted_non_vector_id = "t:Item:item-fingerprinted-non-vector";
    let fingerprinted_non_vector_error = store
        .append_with_index_rows(
            fingerprinted_non_vector_id,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
            &[],
            &[],
            false,
            Some(&fingerprint_a),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            &fingerprinted_non_vector_error,
            PersistenceError::Storage(message)
                if message.contains("stale vector declaration fingerprint")
        ),
        "unexpected fingerprinted non-vector error: {fingerprinted_non_vector_error:?}"
    );
    assert!(
        store
            .read_events(fingerprinted_non_vector_id, 0)
            .await
            .unwrap()
            .is_empty(),
        "a fingerprinted append must not bypass transactional validation"
    );

    let stale_error = store
        .append_with_index_rows(
            persistence_id,
            1,
            &[test_envelope("StaleUpdated", serde_json::json!({}))],
            &[],
            std::slice::from_ref(&row_a),
            true,
            Some(&fingerprint_a),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            &stale_error,
            PersistenceError::Storage(message)
                if message.contains("stale vector declaration fingerprint")
        ),
        "unexpected stale-writer error: {stale_error:?}"
    );

    let batch_error = store
        .append_batch(&[
            PersistenceAppend {
                persistence_id: "t:Audit:audit-stale-writer".to_string(),
                expected_sequence: 0,
                events: vec![test_envelope("Recorded", serde_json::json!({}))],
                vector_rows: Vec::new(),
                reconcile_vectors: false,
                spec_declaration_fingerprint: None,
            },
            PersistenceAppend {
                persistence_id: persistence_id.to_string(),
                expected_sequence: 1,
                events: vec![test_envelope("StaleBatchUpdated", serde_json::json!({}))],
                vector_rows: vec![row_a],
                reconcile_vectors: true,
                spec_declaration_fingerprint: Some(fingerprint_a),
            },
        ])
        .await
        .unwrap_err();
    assert!(
        matches!(
            &batch_error,
            PersistenceError::Storage(message)
                if message.contains("stale vector declaration fingerprint")
        ),
        "unexpected stale batch-writer error: {batch_error:?}"
    );

    assert_eq!(store.read_events(persistence_id, 0).await.unwrap().len(), 1);
    assert!(
        store
            .read_events("t:Audit:audit-stale-writer", 0)
            .await
            .unwrap()
            .is_empty(),
        "batch preflight must reject the stale writer before any journal changes"
    );
    assert!(
        store
            .vector_candidates("t", "Item", "embed-a", "model-a", 10)
            .await
            .unwrap()
            .is_empty(),
        "the stale declaration must not reinstall its vector row"
    );
    assert_eq!(
        store
            .vector_candidates("t", "Item", "embed-b", "model-b", 10)
            .await
            .unwrap()[0]
            .vector,
        row_b.vector
    );
    assert_eq!(
        store.vector_index_backfilled_types("t").await.unwrap(),
        vec![("Item".to_string(), "v2|embed-b".to_string())]
    );
}

#[tokio::test]
async fn deleted_spec_authority_survives_reopen_and_orders_readd() {
    let url = sqlite_test_url("vector-deletion-authority-reopen");
    let ioa_source = "[automaton]\nname = \"Item\"\n# deletion-authority\n";
    let fingerprint = crate::spec_content_hash(ioa_source);
    let store = TursoEventStore::new(&url, None).await.unwrap();
    store
        .upsert_spec(
            "t",
            "Item",
            ioa_source,
            "<Schema Namespace=\"Temper.Tests\" />",
            &fingerprint,
        )
        .await
        .unwrap();
    store.commit_specs("t").await.unwrap();
    let present_generation = store
        .begin_vector_index_reconciliation("t", "Item", "v2|embed", 1, &fingerprint)
        .await
        .unwrap();
    store
        .mark_vector_index_backfilled("t", "Item", present_generation, "v2|embed")
        .await
        .unwrap();

    store.delete_spec("t", "Item").await.unwrap();
    assert!(
        store
            .vector_index_backfilled_types("t")
            .await
            .unwrap()
            .is_empty(),
        "spec deletion must atomically withdraw the old completion claim"
    );
    assert!(
        store
            .begin_vector_index_reconciliation("t", "Item", "v2|embed", 99, &fingerprint)
            .await
            .is_err(),
        "the deleted spec fingerprint must lose authority immediately"
    );
    let absent_generation = store
        .begin_vector_index_reconciliation("t", "Item", "v2|", 1, "absent:v1")
        .await
        .unwrap();
    assert!(absent_generation > present_generation);
    drop(store);

    let reopened = TursoEventStore::new(&url, None).await.unwrap();
    let resumed_generation = reopened
        .begin_vector_index_reconciliation("t", "Item", "v2|", 1, "absent:v1")
        .await
        .unwrap();
    assert_eq!(
        resumed_generation, absent_generation,
        "restart must resume the deletion generation instead of allocating by process-local order"
    );
    reopened
        .mark_vector_index_backfilled("t", "Item", resumed_generation, "v2|")
        .await
        .unwrap();

    reopened
        .upsert_spec(
            "t",
            "Item",
            ioa_source,
            "<Schema Namespace=\"Temper.Tests\" />",
            &fingerprint,
        )
        .await
        .unwrap();
    reopened.commit_specs("t").await.unwrap();
    let readded_generation = reopened
        .begin_vector_index_reconciliation("t", "Item", "v2|embed", 1, &fingerprint)
        .await
        .unwrap();
    assert!(
        readded_generation > resumed_generation,
        "hard delete followed by identical re-add must retain monotonic authority"
    );
    assert!(
        reopened
            .mark_vector_index_backfilled("t", "Item", resumed_generation, "v2|")
            .await
            .is_err(),
        "the completed absence generation must be fenced by the re-add"
    );
}

#[tokio::test]
async fn concurrent_reopen_cannot_miss_declaration_authority_updates() {
    let url = sqlite_test_url("concurrent-trigger-reinstall");
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create initial store");

    for revision in 0..25 {
        let ioa_source = format!("[automaton]\nname = \"Item\"\n# revision {revision}\n");
        let fingerprint = crate::spec_content_hash(&ioa_source);
        let reopen = TursoEventStore::new(&url, None);
        let update = store.upsert_spec(
            "tenant",
            "Item",
            &ioa_source,
            "<Schema Namespace=\"Temper.Tests\" />",
            &fingerprint,
        );
        let (reopened, updated) = tokio::join!(reopen, update);
        reopened.expect("concurrent reopen must finish");
        updated.expect("concurrent spec update must finish");

        let conn = store.configured_connection().await.unwrap();
        let mut rows = conn
            .query(
                "SELECT declaration_fingerprint FROM spec_declaration_authority \
                 WHERE tenant = 'tenant' AND entity_type = 'Item'",
                (),
            )
            .await
            .unwrap();
        let row = rows
            .next()
            .await
            .unwrap()
            .expect("declaration authority row");
        assert_eq!(row.get::<String>(0).unwrap(), fingerprint);
    }
}
