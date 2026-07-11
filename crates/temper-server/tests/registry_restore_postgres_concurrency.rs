//! Real Postgres concurrency contract for ARN-190 quarantine snapshots.

use std::collections::BTreeMap;

use temper_store_postgres::{
    PostgresEventStore, PostgresRegistryQuarantineUpsert, PostgresRegistrySourceSnapshot,
    migration::run_migrations,
};

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const ORDER_CSDL: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");

#[tokio::test]
async fn concurrent_snapshots_keep_one_active_version_when_available() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect Postgres concurrent-snapshot database");
    run_migrations(&pool).await.expect("migrate Postgres");
    let store = PostgresEventStore::new(pool.clone());
    let tenant = format!("arn190-concurrent-{}", uuid::Uuid::new_v4().simple());
    store
        .upsert_spec(&tenant, "Order", ORDER_IOA, ORDER_CSDL, "version-one")
        .await
        .expect("seed concurrent source version one");
    store
        .commit_specs(&tenant)
        .await
        .expect("commit concurrent source version one");
    store
        .upsert_spec(&tenant, "Order", ORDER_IOA, ORDER_CSDL, "version-two")
        .await
        .expect("advance concurrent source version two");
    store
        .commit_specs(&tenant)
        .await
        .expect("commit concurrent source version two");
    let version_one = [PostgresRegistryQuarantineUpsert {
        tenant: &tenant,
        entity_type: "Order",
        spec_version: 1,
        constraint_version: None,
        reason: "invalid_csdl",
        source_kind: "csdl",
        source_line: None,
        source_column: None,
        detail: "concurrent version one",
    }];
    let version_two = [PostgresRegistryQuarantineUpsert {
        tenant: &tenant,
        entity_type: "Order",
        spec_version: 2,
        constraint_version: None,
        reason: "invalid_csdl",
        source_kind: "csdl",
        source_line: None,
        source_column: None,
        detail: "concurrent version two",
    }];
    let source = PostgresRegistrySourceSnapshot {
        spec_versions: BTreeMap::from([((tenant.clone(), "Order".to_string()), 2)]),
        constraint_versions: BTreeMap::from([(tenant.clone(), None)]),
    };

    let (left, right) = tokio::join!(
        store.replace_registry_restore_quarantines_for_tenant(&tenant, &source, &version_one),
        store.replace_registry_restore_quarantines_for_tenant(&tenant, &source, &version_two),
    );
    assert!(
        left.is_ok() || right.is_ok(),
        "at least one concurrent snapshot must commit"
    );
    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM registry_restore_quarantines \
         WHERE tenant = $1 AND entity_type = 'Order' AND resolved_at IS NULL",
    )
    .bind(&tenant)
    .fetch_one(&pool)
    .await
    .expect("count concurrent active quarantines");
    assert_eq!(active, 1, "only one source version may remain active");

    sqlx::query("DELETE FROM registry_restore_quarantines WHERE tenant = $1")
        .bind(&tenant)
        .execute(&pool)
        .await
        .expect("clean concurrent Postgres quarantines");
    sqlx::query("DELETE FROM specs WHERE tenant = $1")
        .bind(&tenant)
        .execute(&pool)
        .await
        .expect("clean concurrent Postgres specs");
}
