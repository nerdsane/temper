use super::*;

async fn item_authority(store: &TursoEventStore) -> (i64, String, i64) {
    let conn = store.configured_connection().await.unwrap();
    let mut rows = conn
        .query(
            "SELECT revision, declaration_fingerprint, present \
             FROM spec_declaration_authority \
             WHERE tenant = 't' AND entity_type = 'Item'",
            (),
        )
        .await
        .unwrap();
    let row = rows
        .next()
        .await
        .unwrap()
        .expect("Item declaration authority");
    (
        row.get::<i64>(0).unwrap(),
        row.get::<String>(1).unwrap(),
        row.get::<i64>(2).unwrap(),
    )
}

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
async fn staged_spec_does_not_advance_authority_until_commit() {
    let store = make_store("vector-staged-declaration-authority").await;
    let csdl = "<Schema Namespace=\"Temper.Tests\" />";
    let ioa_a = "[automaton]\nname = \"Item\"\n# committed-a\n";
    let ioa_b = "[automaton]\nname = \"Item\"\n# staged-b\n";
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
    let authority_a = item_authority(&store).await;

    store
        .upsert_spec("t", "Item", ioa_b, csdl, &fingerprint_b)
        .await
        .unwrap();
    assert_eq!(item_authority(&store).await, authority_a);
    assert_eq!(
        store.vector_index_backfilled_types("t").await.unwrap(),
        vec![("Item".to_string(), "v2|a".to_string())],
        "uncommitted staging must not withdraw the published watermark"
    );

    assert_eq!(store.delete_uncommitted_specs().await.unwrap(), 1);
    assert_eq!(item_authority(&store).await, authority_a);
    assert_eq!(
        store.vector_index_backfilled_types("t").await.unwrap(),
        vec![("Item".to_string(), "v2|a".to_string())],
        "discarding uncommitted staging must not tombstone the live declaration"
    );

    store
        .upsert_spec("t", "Item", ioa_b, csdl, &fingerprint_b)
        .await
        .unwrap();
    store.commit_specs("t").await.unwrap();
    let authority_b = item_authority(&store).await;
    assert!(authority_b.0 > authority_a.0);
    assert_eq!(authority_b.1, fingerprint_b);
    assert_eq!(authority_b.2, 1);
    assert!(
        store
            .vector_index_backfilled_types("t")
            .await
            .unwrap()
            .is_empty(),
        "the false-to-true commit transition must withdraw the old watermark"
    );
}

#[tokio::test]
async fn scoped_commit_does_not_promote_unrelated_staging() {
    let store = make_store("vector-scoped-spec-commit").await;
    let csdl = "<Schema Namespace=\"Temper.Tests\" />";
    let item = "[automaton]\nname = \"Item\"\n";
    let unrelated = "[automaton]\nname = \"Unrelated\"\n";
    let item_fingerprint = crate::spec_content_hash(item);
    let unrelated_fingerprint = crate::spec_content_hash(unrelated);

    store
        .upsert_spec("t", "Item", item, csdl, &item_fingerprint)
        .await
        .unwrap();
    store
        .upsert_spec("t", "Unrelated", unrelated, csdl, &unrelated_fingerprint)
        .await
        .unwrap();
    store
        .commit_verified_spec(
            "t",
            "Item",
            &item_fingerprint,
            csdl,
            crate::TursoSpecVerificationUpdate {
                status: "completed",
                verified: true,
                levels_passed: None,
                levels_total: None,
                verification_result_json: None,
            },
        )
        .await
        .unwrap();

    let committed = store.load_specs().await.unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].entity_type, "Item");
    let conn = store.configured_connection().await.unwrap();
    let unrelated_staged_hash: String = conn
        .query(
            "SELECT content_hash FROM staged_specs WHERE tenant = 't' AND entity_type = 'Unrelated'",
            (),
        )
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .expect("unrelated staged row")
        .get(0)
        .unwrap();
    assert_eq!(unrelated_staged_hash, unrelated_fingerprint);
}

#[tokio::test]
async fn spec_batch_commit_rolls_back_every_promotion_on_mismatch() {
    let store = make_store("vector-batch-spec-rollback").await;
    let csdl = "<Schema Namespace=\"Temper.Tests\" />";
    let item = "[automaton]\nname = \"Item\"\n";
    let issue = "[automaton]\nname = \"Issue\"\n";
    let item_hash = crate::spec_content_hash(item);
    let issue_hash = crate::spec_content_hash(issue);

    store
        .upsert_spec("t", "Item", item, csdl, &item_hash)
        .await
        .unwrap();
    store
        .upsert_spec("t", "Issue", issue, csdl, &issue_hash)
        .await
        .unwrap();
    store
        .commit_spec_batch(
            "t",
            &[
                ("Item", item_hash.as_str(), csdl),
                ("Issue", "wrong-hash", csdl),
            ],
        )
        .await
        .expect_err("one mismatch must roll back the whole batch");

    assert!(store.load_specs().await.unwrap().is_empty());
    let conn = store.configured_connection().await.unwrap();
    let staged: i64 = conn
        .query("SELECT COUNT(*) FROM staged_specs WHERE tenant = 't'", ())
        .await
        .unwrap()
        .next()
        .await
        .unwrap()
        .expect("staged count")
        .get(0)
        .unwrap();
    assert_eq!(staged, 2);
}

#[tokio::test]
async fn verified_commit_rejects_same_type_fingerprint_overwrite() {
    let store = make_store("vector-same-type-verified-commit").await;
    let csdl = "<Schema Namespace=\"Temper.Tests\" />";
    let ioa_a = "[automaton]\nname = \"Item\"\n# verified-a\n";
    let ioa_b = "[automaton]\nname = \"Item\"\n# staged-b\n";
    let fingerprint_a = crate::spec_content_hash(ioa_a);
    let fingerprint_b = crate::spec_content_hash(ioa_b);

    store
        .upsert_spec("t", "Item", ioa_a, csdl, &fingerprint_a)
        .await
        .unwrap();
    store
        .commit_verified_spec(
            "t",
            "Item",
            &fingerprint_a,
            csdl,
            crate::TursoSpecVerificationUpdate {
                status: "completed",
                verified: true,
                levels_passed: None,
                levels_total: None,
                verification_result_json: None,
            },
        )
        .await
        .unwrap();
    store
        .upsert_spec("t", "Item", ioa_b, csdl, &fingerprint_b)
        .await
        .unwrap();
    let error = store
        .commit_verified_spec(
            "t",
            "Item",
            &fingerprint_a,
            csdl,
            crate::TursoSpecVerificationUpdate {
                status: "completed",
                verified: true,
                levels_passed: None,
                levels_total: None,
                verification_result_json: None,
            },
        )
        .await
        .expect_err("verified A must not publish staged B");
    assert!(error.to_string().contains("fingerprint changed"));

    let conn = store.configured_connection().await.unwrap();
    let mut rows = conn
        .query(
            "SELECT content_hash, verified, committed FROM specs \
             WHERE tenant = 't' AND entity_type = 'Item'",
            (),
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().expect("committed A row");
    assert_eq!(row.get::<String>(0).unwrap(), fingerprint_a);
    assert_eq!(row.get::<i64>(1).unwrap(), 1);
    assert_eq!(row.get::<i64>(2).unwrap(), 1);
    let mut staged = conn
        .query(
            "SELECT content_hash FROM staged_specs \
             WHERE tenant = 't' AND entity_type = 'Item'",
            (),
        )
        .await
        .unwrap();
    let staged_row = staged.next().await.unwrap().expect("staged B row");
    assert_eq!(staged_row.get::<String>(0).unwrap(), fingerprint_b);
}

#[tokio::test]
async fn verified_commit_rejects_same_ioa_with_replaced_csdl() {
    let store = make_store("vector-same-ioa-replaced-csdl").await;
    let ioa = "[automaton]\nname = \"Item\"\n";
    let fingerprint = crate::spec_content_hash(ioa);
    let csdl_a = "<Schema Namespace=\"Temper.A\" />";
    let csdl_b = "<Schema Namespace=\"Temper.B\" />";

    store
        .upsert_spec("t", "Item", ioa, csdl_a, &fingerprint)
        .await
        .unwrap();
    store
        .commit_verified_spec(
            "t",
            "Item",
            &fingerprint,
            csdl_a,
            crate::TursoSpecVerificationUpdate {
                status: "completed",
                verified: true,
                levels_passed: None,
                levels_total: None,
                verification_result_json: None,
            },
        )
        .await
        .unwrap();
    store
        .upsert_spec("t", "Item", ioa, csdl_b, &fingerprint)
        .await
        .unwrap();

    store
        .commit_verified_spec(
            "t",
            "Item",
            &fingerprint,
            csdl_a,
            crate::TursoSpecVerificationUpdate {
                status: "completed",
                verified: true,
                levels_passed: None,
                levels_total: None,
                verification_result_json: None,
            },
        )
        .await
        .expect_err("verification of CSDL A must not publish staged CSDL B");

    let conn = store.configured_connection().await.unwrap();
    let mut committed = conn
        .query(
            "SELECT csdl_xml, verified FROM specs \
             WHERE tenant = 't' AND entity_type = 'Item'",
            (),
        )
        .await
        .unwrap();
    let committed_row = committed.next().await.unwrap().expect("committed CSDL A");
    assert_eq!(committed_row.get::<String>(0).unwrap(), csdl_a);
    assert_eq!(committed_row.get::<i64>(1).unwrap(), 1);
    let mut staged = conn
        .query(
            "SELECT csdl_xml FROM staged_specs \
             WHERE tenant = 't' AND entity_type = 'Item'",
            (),
        )
        .await
        .unwrap();
    let staged_row = staged.next().await.unwrap().expect("staged CSDL B");
    assert_eq!(staged_row.get::<String>(0).unwrap(), csdl_b);
}

#[tokio::test]
async fn identical_atomic_app_write_discards_conflicting_staging() {
    let store = make_store("atomic-app-write-discards-staging").await;
    let ioa_a = "[automaton]\nname = \"Item\"\n# app-a\n";
    let ioa_b = "[automaton]\nname = \"Item\"\n# staged-b\n";
    let csdl = "<Schema Namespace=\"Temper.Tests\" />";
    let fingerprint_a = crate::spec_content_hash(ioa_a);
    let fingerprint_b = crate::spec_content_hash(ioa_b);
    let app_specs = [("Item", ioa_a, csdl, fingerprint_a.as_str())];

    store
        .upsert_specs_and_commit("t", &app_specs, None, "test-app")
        .await
        .unwrap();
    store
        .upsert_spec("t", "Item", ioa_b, csdl, &fingerprint_b)
        .await
        .unwrap();

    store
        .upsert_specs_and_commit("t", &app_specs, None, "test-app")
        .await
        .unwrap();

    let committed = store.load_specs().await.unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(
        committed[0].content_hash.as_deref(),
        Some(fingerprint_a.as_str())
    );
    let conn = store.configured_connection().await.unwrap();
    let mut staged = conn
        .query(
            "SELECT 1 FROM staged_specs \
             WHERE tenant = 't' AND entity_type = 'Item'",
            (),
        )
        .await
        .unwrap();
    assert!(
        staged.next().await.unwrap().is_none(),
        "the authoritative app write must discard staged B"
    );
}

#[tokio::test]
async fn full_replacement_tombstones_authority_hidden_by_staged_catalog_row() {
    let store = make_store("vector-staged-full-replacement-tombstone").await;
    let csdl = "<Schema Namespace=\"Temper.Tests\" />";
    let ioa_a = "[automaton]\nname = \"Item\"\n# committed-a\n";
    let ioa_b = "[automaton]\nname = \"Item\"\n# staged-b\n";
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
    let authority_a = item_authority(&store).await;

    store
        .upsert_spec("t", "Item", ioa_b, csdl, &fingerprint_b)
        .await
        .unwrap();
    assert_eq!(item_authority(&store).await, authority_a);

    assert_eq!(
        store
            .persist_spec_catalog_update("t", &[], csdl, &[], true, None)
            .await
            .unwrap(),
        vec!["Item".to_string()]
    );
    let tombstone = item_authority(&store).await;
    assert!(tombstone.0 > authority_a.0);
    assert_eq!(tombstone.1, "absent:v1");
    assert_eq!(tombstone.2, 0);
    assert!(
        store
            .vector_index_backfilled_types("t")
            .await
            .unwrap()
            .is_empty(),
        "full replacement must withdraw the committed declaration even when its catalog row is staged"
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
        let publish = async {
            store
                .upsert_spec(
                    "tenant",
                    "Item",
                    &ioa_source,
                    "<Schema Namespace=\"Temper.Tests\" />",
                    &fingerprint,
                )
                .await?;
            store.commit_specs("tenant").await
        };
        let (reopened, published) = tokio::join!(reopen, publish);
        reopened.expect("concurrent reopen must finish");
        published.expect("concurrent spec publication must finish");

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
