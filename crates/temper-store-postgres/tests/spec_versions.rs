//! The spec commit protocol on PostgreSQL: a type always keeps a committed
//! version, even when installs, commits and startup cleanup interleave.
//!
//! Each test needs `TEMPER_TEST_POSTGRES_URL` pointing at a disposable database.
use std::time::Duration;

use sqlx::PgPool;
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};

const CSDL: &str = "<Schema Namespace=\"Temper.Tests\" />";

async fn setup() -> (PgPool, PostgresSchema, PostgresEventStore) {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL"); // determinism-ok: integration-test configuration
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    let schema =
        PostgresSchema::new(format!("spec_versions_{}", uuid::Uuid::new_v4().simple())).unwrap();
    migration::run_migrations_in_schema(&pool, &schema)
        .await
        .unwrap();
    let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
    (pool, schema, store)
}

async fn teardown(pool: PgPool, schema: PostgresSchema) {
    sqlx::query(&format!(
        "DROP SCHEMA {} CASCADE",
        schema.qualify_sql("{schema}").trim_end_matches('.')
    ))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

/// Install `hash` for Issue and leave it uncommitted.
async fn install(store: &PostgresEventStore, hash: &str) {
    store
        .upsert_spec("t", "Issue", hash, CSDL, hash)
        .await
        .unwrap();
}

/// The Issue version restart would load, if any.
async fn restorable(store: &PostgresEventStore) -> Option<String> {
    store
        .load_specs()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.tenant == "t" && row.entity_type == "Issue")
        .and_then(|row| row.content_hash)
}

/// Wait until a session working in `schema` is blocked on a lock.
async fn until_a_session_waits(pool: &PgPool, schema: &PostgresSchema) {
    let name = schema.qualify_sql("{schema}");
    for _ in 0..200 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE wait_event_type = 'Lock' AND strpos(query, $1) > 0",
        )
        .bind(&name)
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the concurrent install never waited on a lock");
}

/// Run `in_flight` in a transaction left open, start an install of v3 on
/// another connection, let it reach its lock, then commit the transaction.
async fn install_v3_during(
    pool: &PgPool,
    schema: &PostgresSchema,
    store: &PostgresEventStore,
    in_flight: &[&'static str],
) {
    let mut first = pool.begin().await.unwrap();
    for sql in in_flight {
        sqlx::query(&schema.qualify_sql(sql))
            .execute(&mut *first)
            .await
            .unwrap();
    }
    let second = {
        let store = store.clone();
        tokio::spawn(async move { install(&store, "v3").await })
    };
    until_a_session_waits(pool, schema).await;
    first.commit().await.unwrap();
    second.await.unwrap();
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn install_racing_a_commit_keeps_the_newly_committed_version() {
    let (pool, schema, store) = setup().await;
    install(&store, "v1").await;
    assert!(
        store
            .commit_spec_versions("t", &[("Issue", "v1", CSDL)], &[])
            .await
            .unwrap()
    );
    install(&store, "v2").await;

    // Another install's commit of v2, open: it promotes the row and drops the
    // saved v1, as commit_spec_versions does.
    install_v3_during(
        &pool,
        &schema,
        &store,
        &[
            "UPDATE {schema}specs SET committed = true WHERE tenant = 't' AND entity_type = 'Issue'",
            "DELETE FROM {schema}spec_committed_versions WHERE tenant = 't' AND entity_type = 'Issue'",
        ],
    )
    .await;

    assert_eq!(
        restorable(&store).await.as_deref(),
        Some("v2"),
        "v3 is pending, so restart must load v2, which committed first"
    );
    teardown(pool, schema).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn install_racing_startup_cleanup_keeps_the_restored_version() {
    let (pool, schema, store) = setup().await;
    install(&store, "v1").await;
    assert!(
        store
            .commit_spec_versions("t", &[("Issue", "v1", CSDL)], &[])
            .await
            .unwrap()
    );
    install(&store, "v2").await;

    // Another replica's startup cleanup of the abandoned v2, open: it puts v1
    // back and drops the saved copy, as delete_uncommitted_specs does.
    install_v3_during(
        &pool,
        &schema,
        &store,
        &[
            "UPDATE {schema}specs SET ioa_source = 'v1', content_hash = 'v1', committed = true \
             WHERE tenant = 't' AND entity_type = 'Issue'",
            "DELETE FROM {schema}spec_committed_versions WHERE tenant = 't' AND entity_type = 'Issue'",
        ],
    )
    .await;

    assert_eq!(
        restorable(&store).await.as_deref(),
        Some("v1"),
        "v3 is pending, so restart must load v1, which cleanup restored first"
    );
    teardown(pool, schema).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn startup_cleanup_waits_an_hour_before_discarding_an_install() {
    let (pool, schema, store) = setup().await;
    install(&store, "v1").await;
    assert!(
        store
            .commit_spec_versions("t", &[("Issue", "v1", CSDL)], &[])
            .await
            .unwrap()
    );
    install(&store, "v2").await;

    // A young install may be verifying in another replica: left alone, and it
    // can still commit.
    assert_eq!(store.delete_uncommitted_specs().await.unwrap(), 0);
    let row: (String, bool) = sqlx::query_as(&schema.qualify_sql(
        "SELECT content_hash, committed FROM {schema}specs WHERE tenant = 't' AND entity_type = 'Issue'",
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, ("v2".to_string(), false));

    // Abandoned for an hour: v1 comes back and v2 can no longer commit.
    sqlx::query(&schema.qualify_sql(
        "UPDATE {schema}specs SET updated_at = now() - interval '61 minutes' WHERE committed = false",
    ))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(store.delete_uncommitted_specs().await.unwrap(), 1);
    assert_eq!(restorable(&store).await.as_deref(), Some("v1"));
    assert!(
        !store
            .commit_spec_versions("t", &[("Issue", "v2", CSDL)], &[])
            .await
            .unwrap()
    );
    teardown(pool, schema).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn commit_waits_for_an_install_holding_the_row() {
    let (pool, schema, store) = setup().await;
    install(&store, "v2").await;
    assert!(
        store
            .commit_spec_versions("t", &[("Issue", "v2", CSDL)], &[])
            .await
            .unwrap()
    );

    // An install of v3 in flight: it holds the row, has saved v2, and has
    // written v3. A re-commit of v2 must wait for it rather than act on the
    // stale row (and drop the saved v2 once v3 lands).
    let mut install_v3 = pool.begin().await.unwrap();
    for sql in [
        "SELECT 1 FROM {schema}specs WHERE tenant = 't' AND entity_type = 'Issue' FOR UPDATE",
        "INSERT INTO {schema}spec_committed_versions \
         (tenant, entity_type, ioa_source, csdl_xml, content_hash, version, verified, \
          verification_status, levels_passed, levels_total, verification_result, updated_at) \
         SELECT tenant, entity_type, ioa_source, csdl_xml, content_hash, version, verified, \
                verification_status, levels_passed, levels_total, verification_result, updated_at \
         FROM {schema}specs WHERE tenant = 't' AND entity_type = 'Issue'",
        "UPDATE {schema}specs SET ioa_source = 'v3', content_hash = 'v3', committed = false \
         WHERE tenant = 't' AND entity_type = 'Issue'",
    ] {
        sqlx::query(&schema.qualify_sql(sql))
            .execute(&mut *install_v3)
            .await
            .unwrap();
    }
    let recommit = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .commit_spec_versions("t", &[("Issue", "v2", CSDL)], &[])
                .await
                .unwrap()
        })
    };
    until_a_session_waits(&pool, &schema).await;
    install_v3.commit().await.unwrap();
    assert!(
        !recommit.await.unwrap(),
        "the row now holds v3, so v2 is no longer this row's version"
    );
    assert_eq!(restorable(&store).await.as_deref(), Some("v2"));
    teardown(pool, schema).await;
}
