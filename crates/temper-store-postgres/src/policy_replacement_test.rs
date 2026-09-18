//! Opt-in proof against an isolated localhost PostgreSQL database.
use crate::{PostgresEventStore, migration::run_migrations};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::str::FromStr;

#[tokio::test]
#[ignore = "requires TEST_POLICY_DATABASE_URL pointing at disposable localhost morrow_temper"]
async fn conditional_replacement_has_one_winner_on_postgres() {
    let url = std::env::var("TEST_POLICY_DATABASE_URL").unwrap();
    let options = PgConnectOptions::from_str(&url).unwrap();
    assert!(matches!(options.get_host(), "127.0.0.1" | "localhost"));
    assert_eq!(options.get_database(), Some("morrow_temper"));
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .unwrap();
    run_migrations(&pool).await.unwrap();
    let store = PostgresEventStore::new(pool.clone());
    let tenant = format!("policy-test-{}", uuid::Uuid::new_v4());
    let other = format!("other-{tenant}");
    store
        .save_policy(&tenant, "legacy", "old", "fixture")
        .await
        .unwrap();
    store
        .save_policy(&other, "legacy", "unrelated", "fixture")
        .await
        .unwrap();
    let expected = format!("{:x}", Sha256::digest(b"old"));
    let (first, second) = tokio::join!(
        store.replace_policy_if_hash(&tenant, "legacy", &expected, "first", "human"),
        store.replace_policy_if_hash(&tenant, "legacy", &expected, "second", "human"),
    );
    assert_ne!(
        first.unwrap(),
        second.unwrap(),
        "Exactly one same-version proposal may commit"
    );
    assert!(
        !store
            .replace_policy_if_hash(&tenant, "legacy", &expected, "stale", "human")
            .await
            .unwrap()
    );
    let rows = store.load_policies_for_tenant(&tenant).await.unwrap();
    store
        .toggle_policy_enabled(&tenant, "legacy", false)
        .await
        .unwrap();
    assert!(
        !store
            .replace_policy_if_hash(&tenant, "legacy", &rows[0].policy_hash, "disabled", "human")
            .await
            .unwrap()
    );
    assert!(
        !store
            .replace_policy_if_hash(&tenant, "absent", &expected, "created", "human")
            .await
            .unwrap()
    );
    assert_eq!(
        store.load_policies_for_tenant(&other).await.unwrap()[0].cedar_text,
        "unrelated"
    );
    store.delete_policy(&tenant, "legacy").await.unwrap();
    store.delete_policy(&other, "legacy").await.unwrap();
    pool.close().await;
}
