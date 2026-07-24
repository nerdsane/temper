use std::time::Duration;

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

fn test_envelope(event_type: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "authority-test".to_string(),
        },
    }
}

#[test]
fn fresh_writers_establish_one_authority_and_cannot_reclaim_its_tombstone() {
    let Some(database_url) = database_url("fresh_writers_establish_one_authority") else {
        return;
    };

    sqlx::__rt::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let store = PostgresEventStore::new(pool);
        let tenant = format!("tenant-authority-bootstrap-{}", uuid::Uuid::new_v4());
        let fingerprint_a = spec_content_fingerprint("declaration-a");
        let fingerprint_b = spec_content_fingerprint("declaration-b");

        let writer_a = store.clone();
        let tenant_a = tenant.clone();
        let fingerprint_a_task = fingerprint_a.clone();
        let writer_a = sqlx::__rt::spawn(async move {
            writer_a
                .append_with_index_rows(
                    &format!("{tenant_a}:Item:item-a"),
                    0,
                    &[test_envelope("CreatedByA")],
                    &[],
                    &[],
                    false,
                    Some(&fingerprint_a_task),
                )
                .await
        });
        let writer_b = store.clone();
        let tenant_b = tenant.clone();
        let fingerprint_b_task = fingerprint_b.clone();
        let writer_b = sqlx::__rt::spawn(async move {
            writer_b
                .append_with_index_rows(
                    &format!("{tenant_b}:Item:item-b"),
                    0,
                    &[test_envelope("CreatedByB")],
                    &[],
                    &[],
                    false,
                    Some(&fingerprint_b_task),
                )
                .await
        });
        let results = [writer_a.await, writer_b.await];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(
                    result,
                    Err(PersistenceError::Storage(message))
                        if message.contains("stale spec declaration fingerprint")
                ))
                .count(),
            1
        );

        let authority: (i64, String, bool) = crate::dbm::postgres_query_as!(
            "SELECT revision, declaration_fingerprint, present \
             FROM spec_declaration_authority \
             WHERE tenant = $1 AND entity_type = 'Item'",
        )
        .bind(&tenant)
        .fetch_one(store.pool())
        .await
        .expect("read authority");
        assert_eq!(authority.0, 1);
        assert!(authority.1 == fingerprint_a || authority.1 == fingerprint_b);
        assert!(authority.2);

        store
            .delete_spec(&tenant, "Item")
            .await
            .expect("tombstone compatibility authority");
        let stale = store
            .append_with_index_rows(
                &format!("{tenant}:Item:item-after-delete"),
                0,
                &[test_envelope("Created")],
                &[],
                &[],
                false,
                Some(&authority.1),
            )
            .await
            .expect_err("tombstone cannot be reclaimed");
        assert!(matches!(
            stale,
            PersistenceError::Storage(message)
                if message.contains("stale spec declaration fingerprint")
        ));
    });
}

#[test]
fn fresh_reconciliation_bootstraps_revision_one_not_the_caller_revision() {
    let Some(database_url) = database_url("fresh_reconciliation_bootstraps_revision_one") else {
        return;
    };

    sqlx::__rt::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let store = PostgresEventStore::new(pool);
        let tenant = format!("tenant-reconciliation-bootstrap-{}", uuid::Uuid::new_v4());
        let fingerprint = spec_content_fingerprint("fresh-vector-declaration");

        assert_eq!(
            store
                .begin_vector_index_reconciliation(
                    &tenant,
                    "Item",
                    "v2|embed",
                    u64::MAX,
                    &fingerprint,
                )
                .await
                .expect("bootstrap reconciliation"),
            1
        );
        let authority: (i64, String, bool) = crate::dbm::postgres_query_as!(
            "SELECT revision, declaration_fingerprint, present \
             FROM spec_declaration_authority \
             WHERE tenant = $1 AND entity_type = 'Item'",
        )
        .bind(&tenant)
        .fetch_one(store.pool())
        .await
        .expect("read authority");
        assert_eq!(authority, (1, fingerprint, true));
    });
}

#[test]
fn staged_spec_does_not_advance_authority_until_commit() {
    let Some(database_url) = database_url("staged_spec_does_not_advance_authority") else {
        return;
    };

    sqlx::__rt::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let store = PostgresEventStore::new(pool);
        let tenant = format!("tenant-staged-authority-{}", uuid::Uuid::new_v4());
        let ioa_a = "[automaton]\nname = \"Item\"\n# committed-a\n";
        let ioa_b = "[automaton]\nname = \"Item\"\n# staged-b\n";
        let fingerprint_a = spec_content_fingerprint(ioa_a);
        let fingerprint_b = spec_content_fingerprint(ioa_b);
        let csdl = "<Schema Namespace=\"Temper.Tests\" />";

        store
            .upsert_spec(&tenant, "Item", ioa_a, csdl, &fingerprint_a)
            .await
            .expect("stage A");
        store.commit_specs(&tenant).await.expect("commit A");
        let generation_a = store
            .begin_vector_index_reconciliation(&tenant, "Item", "v2|a", 1, &fingerprint_a)
            .await
            .expect("begin A");
        store
            .mark_vector_index_backfilled(&tenant, "Item", generation_a, "v2|a")
            .await
            .expect("publish A watermark");
        let authority_a: (i64, String, bool) = crate::dbm::postgres_query_as!(
            "SELECT revision, declaration_fingerprint, present \
             FROM spec_declaration_authority \
             WHERE tenant = $1 AND entity_type = 'Item'",
        )
        .bind(&tenant)
        .fetch_one(store.pool())
        .await
        .expect("read A authority");

        store
            .upsert_spec(&tenant, "Item", ioa_b, csdl, &fingerprint_b)
            .await
            .expect("stage B");
        let staged_authority: (i64, String, bool) = crate::dbm::postgres_query_as!(
            "SELECT revision, declaration_fingerprint, present \
             FROM spec_declaration_authority \
             WHERE tenant = $1 AND entity_type = 'Item'",
        )
        .bind(&tenant)
        .fetch_one(store.pool())
        .await
        .expect("read staged authority");
        assert_eq!(staged_authority, authority_a);
        assert_eq!(
            store
                .vector_index_backfilled_types(&tenant)
                .await
                .expect("A watermark during staging"),
            vec![("Item".to_string(), "v2|a".to_string())]
        );

        crate::dbm::postgres_query!(
            "DELETE FROM specs WHERE tenant = $1 AND entity_type = 'Item' AND committed = false",
        )
        .bind(&tenant)
        .execute(store.pool())
        .await
        .expect("discard staged B");
        let discarded_authority: (i64, String, bool) = crate::dbm::postgres_query_as!(
            "SELECT revision, declaration_fingerprint, present \
             FROM spec_declaration_authority \
             WHERE tenant = $1 AND entity_type = 'Item'",
        )
        .bind(&tenant)
        .fetch_one(store.pool())
        .await
        .expect("read authority after discard");
        assert_eq!(discarded_authority, authority_a);

        store
            .upsert_spec(&tenant, "Item", ioa_b, csdl, &fingerprint_b)
            .await
            .expect("restage B");
        store.commit_specs(&tenant).await.expect("commit B");
        let authority_b: (i64, String, bool) = crate::dbm::postgres_query_as!(
            "SELECT revision, declaration_fingerprint, present \
             FROM spec_declaration_authority \
             WHERE tenant = $1 AND entity_type = 'Item'",
        )
        .bind(&tenant)
        .fetch_one(store.pool())
        .await
        .expect("read B authority");
        assert!(authority_b.0 > authority_a.0);
        assert_eq!(authority_b.1, fingerprint_b);
        assert!(authority_b.2);
        assert!(
            store
                .vector_index_backfilled_types(&tenant)
                .await
                .expect("watermark after B commit")
                .is_empty(),
            "the false-to-true commit transition must withdraw A's watermark"
        );
    });
}

#[test]
fn full_replacement_tombstones_authority_hidden_by_staged_catalog_row() {
    let Some(database_url) =
        database_url("full_replacement_tombstones_authority_hidden_by_staged_catalog_row")
    else {
        return;
    };

    sqlx::__rt::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let store = PostgresEventStore::new(pool);
        let tenant = format!("tenant-staged-replacement-{}", uuid::Uuid::new_v4());
        let ioa_a = "[automaton]\nname = \"Item\"\n# committed-a\n";
        let ioa_b = "[automaton]\nname = \"Item\"\n# staged-b\n";
        let fingerprint_a = spec_content_fingerprint(ioa_a);
        let fingerprint_b = spec_content_fingerprint(ioa_b);
        let csdl = "<Schema Namespace=\"Temper.Tests\" />";

        store
            .upsert_spec(&tenant, "Item", ioa_a, csdl, &fingerprint_a)
            .await
            .expect("stage A");
        store.commit_specs(&tenant).await.expect("commit A");
        let generation_a = store
            .begin_vector_index_reconciliation(&tenant, "Item", "v2|a", 1, &fingerprint_a)
            .await
            .expect("begin A");
        store
            .mark_vector_index_backfilled(&tenant, "Item", generation_a, "v2|a")
            .await
            .expect("publish A watermark");
        let authority_a: (i64, String, bool) = crate::dbm::postgres_query_as!(
            "SELECT revision, declaration_fingerprint, present \
             FROM spec_declaration_authority \
             WHERE tenant = $1 AND entity_type = 'Item'",
        )
        .bind(&tenant)
        .fetch_one(store.pool())
        .await
        .expect("read A authority");

        store
            .upsert_spec(&tenant, "Item", ioa_b, csdl, &fingerprint_b)
            .await
            .expect("stage B");
        let staged_authority: (i64, String, bool) = crate::dbm::postgres_query_as!(
            "SELECT revision, declaration_fingerprint, present \
             FROM spec_declaration_authority \
             WHERE tenant = $1 AND entity_type = 'Item'",
        )
        .bind(&tenant)
        .fetch_one(store.pool())
        .await
        .expect("read staged authority");
        assert_eq!(staged_authority, authority_a);

        assert_eq!(
            store
                .persist_spec_catalog_update(&tenant, &[], csdl, &[], true, None)
                .await
                .expect("replace with empty catalog"),
            vec!["Item".to_string()]
        );
        let tombstone: (i64, String, bool) = crate::dbm::postgres_query_as!(
            "SELECT revision, declaration_fingerprint, present \
             FROM spec_declaration_authority \
             WHERE tenant = $1 AND entity_type = 'Item'",
        )
        .bind(&tenant)
        .fetch_one(store.pool())
        .await
        .expect("read tombstone");
        assert!(tombstone.0 > authority_a.0);
        assert_eq!(tombstone.1, "absent:v1");
        assert!(!tombstone.2);
        assert!(
            store
                .vector_index_backfilled_types(&tenant)
                .await
                .expect("watermarks after omission")
                .is_empty(),
            "full replacement must withdraw the committed declaration even when its catalog row is staged"
        );
    });
}

#[test]
fn verified_commit_rejects_same_type_fingerprint_overwrite() {
    let Some(database_url) = database_url("verified_commit_rejects_same_type_overwrite") else {
        return;
    };

    sqlx::__rt::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let store = PostgresEventStore::new(pool);
        let tenant = format!("tenant-same-type-commit-{}", uuid::Uuid::new_v4());
        let ioa_a = "[automaton]\nname = \"Item\"\n# committed-a\n";
        let ioa_b = "[automaton]\nname = \"Item\"\n# staged-b\n";
        let fingerprint_a = spec_content_fingerprint(ioa_a);
        let fingerprint_b = spec_content_fingerprint(ioa_b);
        let csdl = "<Schema Namespace=\"Temper.Tests\" />";

        store
            .upsert_spec(&tenant, "Item", ioa_a, csdl, &fingerprint_a)
            .await
            .expect("stage A");
        store
            .commit_verified_spec(
                &tenant,
                "Item",
                &fingerprint_a,
                crate::PostgresSpecVerificationUpdate {
                    status: "completed",
                    verified: true,
                    levels_passed: None,
                    levels_total: None,
                    verification_result_json: None,
                },
            )
            .await
            .expect("commit verified A");
        store
            .upsert_spec(&tenant, "Item", ioa_b, csdl, &fingerprint_b)
            .await
            .expect("stage B over A");

        let error = store
            .commit_verified_spec(
                &tenant,
                "Item",
                &fingerprint_a,
                crate::PostgresSpecVerificationUpdate {
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

        let staged_b: (String, bool, bool) = crate::dbm::postgres_query_as!(
            "SELECT content_hash, verified, committed FROM specs \
             WHERE tenant = $1 AND entity_type = 'Item'",
        )
        .bind(&tenant)
        .fetch_one(store.pool())
        .await
        .expect("read staged B");
        assert_eq!(staged_b, (fingerprint_b, false, false));
        let authority: (String, bool) = crate::dbm::postgres_query_as!(
            "SELECT declaration_fingerprint, present FROM spec_declaration_authority \
             WHERE tenant = $1 AND entity_type = 'Item'",
        )
        .bind(&tenant)
        .fetch_one(store.pool())
        .await
        .expect("read committed A authority");
        assert_eq!(authority, (fingerprint_a, true));
    });
}

#[test]
fn verification_cache_ignores_staged_specs_until_commit() {
    let Some(database_url) = database_url("verification_cache_ignores_staged_specs_until_commit")
    else {
        return;
    };

    sqlx::__rt::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let store = PostgresEventStore::new(pool);
        let tenant = format!("tenant-staged-cache-{}", uuid::Uuid::new_v4());
        let ioa_source = "[automaton]\nname = \"Issue\"\n";
        let csdl = "<Schema Namespace=\"Temper.Tests\" />";
        let content_hash = spec_content_fingerprint(ioa_source);

        store
            .upsert_spec(&tenant, "Issue", ioa_source, csdl, &content_hash)
            .await
            .expect("stage Issue");
        store
            .persist_spec_verification(
                &tenant,
                "Issue",
                crate::PostgresSpecVerificationUpdate {
                    status: "passed",
                    verified: true,
                    levels_passed: Some(1),
                    levels_total: Some(1),
                    verification_result_json: Some(r#"{"all_passed":true}"#),
                },
            )
            .await
            .expect("verify staged Issue");

        assert!(
            !store
                .load_verification_cache(&tenant)
                .await
                .expect("load staged cache")
                .contains_key("Issue"),
            "staged verification must not make bootstrap skip durable publication"
        );

        store.commit_specs(&tenant).await.expect("commit Issue");
        assert_eq!(
            store
                .load_verification_cache(&tenant)
                .await
                .expect("load committed cache")
                .get("Issue"),
            Some(&(content_hash, true))
        );
    });
}

#[test]
fn existing_authority_writers_share_the_fence_while_spec_mutation_waits() {
    let Some(database_url) = database_url("existing_authority_writers_share_the_fence") else {
        return;
    };

    sqlx::__rt::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let store = PostgresEventStore::new(pool);
        let tenant = format!("tenant-shared-authority-{}", uuid::Uuid::new_v4());
        let ioa_a = "[automaton]\nname = \"Item\"\n# shared-a\n";
        let ioa_b = "[automaton]\nname = \"Item\"\n# shared-b\n";
        let fingerprint_a = spec_content_fingerprint(ioa_a);
        let fingerprint_b = spec_content_fingerprint(ioa_b);
        let csdl = "<Schema Namespace=\"Temper.Tests\" />";
        store
            .upsert_spec(&tenant, "Item", ioa_a, csdl, &fingerprint_a)
            .await
            .expect("persist authority A");
        store.commit_specs(&tenant).await.expect("commit A");

        let mut writer_a = store.pool().begin().await.expect("writer A transaction");
        PostgresEventStore::validate_live_spec_declaration(
            &mut writer_a,
            &tenant,
            "Item",
            &fingerprint_a,
        )
        .await
        .expect("writer A shared fence");

        let mut writer_b = store.pool().begin().await.expect("writer B transaction");
        sqlx::__rt::timeout(
            Duration::from_secs(1),
            PostgresEventStore::validate_live_spec_declaration(
                &mut writer_b,
                &tenant,
                "Item",
                &fingerprint_a,
            ),
        )
        .await
        .expect("existing-authority writer B must not serialize behind writer A")
        .expect("writer B shared fence");

        let mut mutation = {
            let mutation_store = store.clone();
            let mutation_tenant = tenant.clone();
            let mutation_fingerprint = fingerprint_b.clone();
            sqlx::__rt::spawn(async move {
                mutation_store
                    .upsert_spec(&mutation_tenant, "Item", ioa_b, csdl, &mutation_fingerprint)
                    .await?;
                mutation_store.commit_specs(&mutation_tenant).await
            })
        };
        assert!(
            sqlx::__rt::timeout(Duration::from_millis(100), &mut mutation)
                .await
                .is_err(),
            "spec publication must wait for writer A and writer B"
        );
        writer_a.commit().await.expect("commit writer A");
        assert!(
            sqlx::__rt::timeout(Duration::from_millis(100), &mut mutation)
                .await
                .is_err(),
            "spec publication must still wait for writer B"
        );
        writer_b.commit().await.expect("commit writer B");
        mutation
            .await
            .expect("spec publication after both shared fences");
    });
}
