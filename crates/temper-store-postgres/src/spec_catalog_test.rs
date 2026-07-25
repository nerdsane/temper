use super::*;
use crate::migration::run_migrations;

#[test]
fn replacement_enumeration_includes_staged_only_entity_types() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        tracing::warn!("skipping Postgres integration test: DATABASE_URL is not set");
        return;
    };

    sqlx::__rt::test_block_on(async {
        let pool = sqlx::PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let store = PostgresEventStore::new(pool);
        let tenant = format!("tenant-staged-enumeration-{}", uuid::Uuid::new_v4());

        store
            .upsert_spec(
                &tenant,
                "StagedOnly",
                "[automaton]\nname = \"StagedOnly\"\n",
                "<Schema Namespace=\"Temper.Tests\" />",
                "staged-only-fingerprint",
            )
            .await
            .expect("stage catalog-only type");

        assert_eq!(
            store
                .spec_replacement_entity_types(&tenant)
                .await
                .expect("enumerate replacement types"),
            vec!["StagedOnly".to_string()]
        );
    });
}

#[test]
fn concurrent_replica_replacements_commit_one_complete_catalog() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        tracing::warn!("skipping Postgres integration test: DATABASE_URL is not set");
        return;
    };

    sqlx::__rt::test_block_on(async {
        let pool = sqlx::PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let store_a = PostgresEventStore::new(pool.clone());
        let store_b = PostgresEventStore::new(pool.clone());
        let reader = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-concurrent-catalog-{}", uuid::Uuid::new_v4());
        let csdl = "<Schema Namespace=\"Temper.Tests\" />";
        let source_a = "[automaton]\nname = \"ItemA\"\n";
        let source_b = "[automaton]\nname = \"ItemB\"\n";
        let tenant_a = tenant.clone();
        let replacement_a = sqlx::__rt::spawn(async move {
            let specs = [("ItemA", source_a, "fingerprint-a")];
            store_a
                .persist_spec_catalog_update(&tenant_a, &specs, csdl, &[], true, None)
                .await
        });
        let tenant_b = tenant.clone();
        let replacement_b = sqlx::__rt::spawn(async move {
            let specs = [("ItemB", source_b, "fingerprint-b")];
            store_b
                .persist_spec_catalog_update(&tenant_b, &specs, csdl, &[], true, None)
                .await
        });
        replacement_a
            .await
            .expect("first replica replacement must commit");
        replacement_b
            .await
            .expect("second replica replacement must commit");

        let committed: Vec<String> = crate::dbm::postgres_query_scalar!(
            "SELECT entity_type FROM specs \
             WHERE tenant = $1 AND committed = true ORDER BY entity_type",
        )
        .bind(&tenant)
        .fetch_all(&pool)
        .await
        .expect("load committed catalog");
        assert!(
            committed == ["ItemA"] || committed == ["ItemB"],
            "the final durable catalog must be one serialized replacement, got {committed:?}"
        );
        assert_eq!(
            reader
                .spec_replacement_entity_types(&tenant)
                .await
                .expect("load present authority"),
            committed,
            "the authority rows must recover the same single catalog"
        );
    });
}

#[test]
fn merge_without_constraints_preserves_them_across_restart_and_replace_clears_them() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        tracing::warn!("skipping Postgres integration test: DATABASE_URL is not set");
        return;
    };

    sqlx::__rt::test_block_on(async {
        let pool = sqlx::PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-merge-constraints-{}", uuid::Uuid::new_v4());
        let csdl = "<Schema Namespace=\"Temper.Tests\" />";
        let source_a = "[automaton]\nname = \"ItemA\"\n";
        let source_b = "[automaton]\nname = \"ItemB\"\n";
        let specs_a = [("ItemA", source_a, "fingerprint-a")];
        let specs_b = [("ItemB", source_b, "fingerprint-b")];
        let constraints = r#"version = 1
default_delete_policy = "restrict"

[[invariant]]
name = "payment_must_be_captured"
kind = "hard"
on = "Order.Submit"
assert = 'related(Payment, payment_id).status in ["Captured"]'
"#;

        store
            .persist_spec_catalog_update(&tenant, &specs_a, csdl, &[], true, Some(constraints))
            .await
            .expect("seed replacement with constraints");
        store
            .persist_spec_catalog_update(&tenant, &specs_b, csdl, &[], false, None)
            .await
            .expect("merge without constraints");
        drop(store);
        drop(pool);

        let reopened_pool = sqlx::PgPool::connect(&database_url)
            .await
            .expect("reconnect after merge");
        let preserved: String = crate::dbm::postgres_query_scalar!(
            "SELECT cross_invariants_toml FROM tenant_constraints WHERE tenant = $1",
        )
        .bind(&tenant)
        .fetch_one(&reopened_pool)
        .await
        .expect("constraints must survive merge restart");
        assert_eq!(preserved, constraints);

        let reopened = PostgresEventStore::new(reopened_pool.clone());
        reopened
            .persist_spec_catalog_update(&tenant, &specs_a, csdl, &[], true, None)
            .await
            .expect("constraint-free replacement");
        let cleared: Option<String> = crate::dbm::postgres_query_scalar!(
            "SELECT cross_invariants_toml FROM tenant_constraints WHERE tenant = $1",
        )
        .bind(&tenant)
        .fetch_optional(&reopened_pool)
        .await
        .expect("read cleared constraints");
        assert_eq!(cleared, None);
    });
}
