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
                    .await
            })
        };
        assert!(
            sqlx::__rt::timeout(Duration::from_millis(100), &mut mutation)
                .await
                .is_err(),
            "spec mutation must wait for writer A and writer B"
        );
        writer_a.commit().await.expect("commit writer A");
        assert!(
            sqlx::__rt::timeout(Duration::from_millis(100), &mut mutation)
                .await
                .is_err(),
            "spec mutation must still wait for writer B"
        );
        writer_b.commit().await.expect("commit writer B");
        mutation
            .await
            .expect("spec mutation after both shared fences");
    });
}
