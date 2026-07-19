use super::*;
use crate::migration::run_migrations;

fn database_url(test_name: &str) -> Option<String> {
    match std::env::var("DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            tracing::warn!(
                test_name,
                "skipping Postgres integration test: DATABASE_URL is not set"
            );
            None
        }
    }
}

fn envelope(event_type: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "vector-test".to_string(),
        },
    }
}

fn vector(decl_name: &str, x: f32, y: f32) -> EntityVectorRow {
    EntityVectorRow {
        decl_name: decl_name.to_string(),
        model_tag: "m1".to_string(),
        vector: vec![x, y],
    }
}

#[test]
fn reconciliation_generations_fence_stale_rows_deletion_and_readd() {
    let Some(database_url) = database_url("reconciliation_generations_fence_stale_rows") else {
        return;
    };

    sqlx::__rt::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let store = PostgresEventStore::new(pool);
        let tenant = format!("tenant-vector-generation-{}", uuid::Uuid::new_v4());
        let persistence_id = format!("{tenant}:Item:item-1");
        let ioa_a = "[automaton]\nname = \"Item\"\n# vector-a\n";
        let ioa_b = "[automaton]\nname = \"Item\"\n# vector-b\n";
        let fingerprint_a = spec_content_fingerprint(ioa_a);
        let fingerprint_b = spec_content_fingerprint(ioa_b);
        let csdl = "<Schema Namespace=\"Temper.Tests\" />";

        store
            .upsert_spec(&tenant, "Item", ioa_a, csdl, &fingerprint_a)
            .await
            .expect("persist A");
        store.commit_specs(&tenant).await.expect("commit A");
        let generation_a = store
            .begin_vector_index_reconciliation(&tenant, "Item", "v2|a", 1, &fingerprint_a)
            .await
            .expect("begin A");
        store
            .mark_vector_index_backfilled(&tenant, "Item", generation_a, "v2|a")
            .await
            .expect("publish A");

        store
            .upsert_spec(&tenant, "Item", ioa_b, csdl, &fingerprint_b)
            .await
            .expect("persist B");
        store.commit_specs(&tenant).await.expect("commit B");
        let generation_b = store
            .begin_vector_index_reconciliation(&tenant, "Item", "v2|b", 2, &fingerprint_b)
            .await
            .expect("begin B");
        assert!(generation_b > generation_a);
        assert!(
            store
                .vector_index_backfilled_types(&tenant)
                .await
                .expect("watermarks")
                .is_empty(),
            "B must withdraw A's completion before rebuilding"
        );
        assert!(
            store
                .begin_vector_index_reconciliation(&tenant, "Item", "v2|a", 99, &fingerprint_a)
                .await
                .is_err(),
            "caller revision cannot let stale A reclaim B"
        );

        store
            .append_with_index_rows(
                &persistence_id,
                0,
                &[envelope("Created")],
                &[],
                &[vector("b", 0.0, 1.0)],
                true,
                Some(&fingerprint_b),
            )
            .await
            .expect("append live B vector");
        store
            .backfill_entity_vectors(
                &tenant,
                "Item",
                "item-1",
                generation_b,
                0,
                &[vector("a", 1.0, 0.0)],
            )
            .await
            .expect("ignore older replay");
        assert!(
            store
                .vector_candidates(&tenant, "Item", "a", "m1", 10)
                .await
                .expect("A candidates")
                .is_empty()
        );

        store
            .append_with_index_rows(
                &persistence_id,
                1,
                &[envelope("Deleted")],
                &[],
                &[],
                true,
                Some(&fingerprint_b),
            )
            .await
            .expect("purge live vectors");
        store
            .backfill_entity_vectors(
                &tenant,
                "Item",
                "item-1",
                generation_b,
                1,
                &[vector("b", 0.0, 1.0)],
            )
            .await
            .expect("ignore resurrection at delete fence");
        assert!(
            store
                .vector_candidates(&tenant, "Item", "b", "m1", 10)
                .await
                .expect("B candidates")
                .is_empty()
        );

        store.delete_spec(&tenant, "Item").await.expect("delete B");
        let absent_generation = store
            .begin_vector_index_reconciliation(
                &tenant,
                "Item",
                "v2|",
                1,
                ABSENT_DECLARATION_FINGERPRINT,
            )
            .await
            .expect("begin absence");
        store
            .upsert_spec(&tenant, "Item", ioa_a, csdl, &fingerprint_a)
            .await
            .expect("re-add A");
        store.commit_specs(&tenant).await.expect("commit re-add");
        let readded_generation = store
            .begin_vector_index_reconciliation(&tenant, "Item", "v2|a", 1, &fingerprint_a)
            .await
            .expect("begin re-added A");
        assert!(readded_generation > absent_generation);
    });
}

#[test]
fn stale_live_writer_cannot_advance_single_or_batch_journals() {
    let Some(database_url) = database_url("stale_live_writer_cannot_advance_journals") else {
        return;
    };

    sqlx::__rt::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let store = PostgresEventStore::new(pool);
        let tenant = format!("tenant-stale-vector-writer-{}", uuid::Uuid::new_v4());
        let item_id = format!("{tenant}:Item:item-1");
        let audit_id = format!("{tenant}:Audit:audit-1");
        let ioa_a = "[automaton]\nname = \"Item\"\n# writer-a\n";
        let ioa_b = "[automaton]\nname = \"Item\"\n# writer-b\n";
        let fingerprint_a = spec_content_fingerprint(ioa_a);
        let fingerprint_b = spec_content_fingerprint(ioa_b);
        let csdl = "<Schema Namespace=\"Temper.Tests\" />";

        store
            .upsert_spec(&tenant, "Item", ioa_a, csdl, &fingerprint_a)
            .await
            .expect("persist A");
        store.commit_specs(&tenant).await.expect("commit A");
        store
            .append_with_index_rows(
                &item_id,
                0,
                &[envelope("Created")],
                &[],
                &[vector("a", 1.0, 0.0)],
                true,
                Some(&fingerprint_a),
            )
            .await
            .expect("append A");

        store
            .upsert_spec(&tenant, "Item", ioa_b, csdl, &fingerprint_b)
            .await
            .expect("persist B");
        store.commit_specs(&tenant).await.expect("commit B");
        let generation_b = store
            .begin_vector_index_reconciliation(&tenant, "Item", "v2|b", 2, &fingerprint_b)
            .await
            .expect("begin B");
        store
            .backfill_entity_vectors(
                &tenant,
                "Item",
                "item-1",
                generation_b,
                1,
                &[vector("b", 0.0, 1.0)],
            )
            .await
            .expect("install B");

        let stale_single = store
            .append_with_index_rows(
                &item_id,
                1,
                &[envelope("StaleUpdated")],
                &[],
                &[],
                false,
                Some(&fingerprint_a),
            )
            .await
            .expect_err("stale non-vector write");
        assert!(matches!(
            stale_single,
            PersistenceError::Storage(message)
                if message.contains("stale spec declaration fingerprint")
        ));

        let stale_batch = store
            .append_batch(&[
                PersistenceAppend {
                    persistence_id: audit_id.clone(),
                    expected_sequence: 0,
                    events: vec![envelope("Recorded")],
                    vector_rows: Vec::new(),
                    reconcile_vectors: false,
                    spec_declaration_fingerprint: None,
                },
                PersistenceAppend {
                    persistence_id: item_id.clone(),
                    expected_sequence: 1,
                    events: vec![envelope("StaleBatchUpdated")],
                    vector_rows: vec![vector("a", 1.0, 0.0)],
                    reconcile_vectors: true,
                    spec_declaration_fingerprint: Some(fingerprint_a),
                },
            ])
            .await
            .expect_err("stale batch writer");
        assert!(matches!(
            stale_batch,
            PersistenceError::Storage(message)
                if message.contains("stale spec declaration fingerprint")
        ));
        assert_eq!(store.read_events(&item_id, 0).await.unwrap().len(), 1);
        assert!(store.read_events(&audit_id, 0).await.unwrap().is_empty());
        assert!(
            store
                .vector_candidates(&tenant, "Item", "a", "m1", 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .vector_candidates(&tenant, "Item", "b", "m1", 10)
                .await
                .unwrap()[0]
                .vector,
            vec![0.0, 1.0]
        );
    });
}
