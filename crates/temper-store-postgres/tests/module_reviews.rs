//! What the newest review of an app's modules said, on PostgreSQL.
//!
//! Each test needs `TEMPER_TEST_POSTGRES_URL` pointing at a disposable database.
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn the_newest_review_of_an_app_is_kept_for_its_tenant() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL"); // determinism-ok: integration-test configuration
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let schema =
        PostgresSchema::new(format!("module_reviews_{}", uuid::Uuid::new_v4().simple())).unwrap();
    migration::run_migrations_in_schema(&pool, &schema)
        .await
        .unwrap();
    let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());

    assert_eq!(store.load_module_review("t", "app").await.unwrap(), None);

    store
        .save_module_review("t", "app", "c1", r#"{"warnings":[]}"#)
        .await
        .unwrap();
    store
        .save_module_review("t", "app", "c2", r#"{"warnings":[1]}"#)
        .await
        .unwrap();

    assert_eq!(
        store.load_module_review("t", "app").await.unwrap(),
        Some(("c2".to_string(), r#"{"warnings":[1]}"#.to_string()))
    );
    assert_eq!(
        store.load_module_review("other", "app").await.unwrap(),
        None
    );
    assert_eq!(store.load_module_review("t", "other").await.unwrap(), None);

    sqlx::query(&format!(
        "DROP SCHEMA {} CASCADE",
        schema.qualify_sql("{schema}").trim_end_matches('.')
    ))
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}
