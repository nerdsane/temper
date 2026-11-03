//! A rollback boots: a release runs on a schema that a newer release migrated.
use std::borrow::Cow;

use sqlx::migrate::{Migration, MigrationType, Migrator};
use sqlx::postgres::PgPoolOptions;

use super::{migrator, run_in_schema};
use crate::PostgresSchema;

/// This release's migrations plus one a later release would add.
fn next_release() -> Migrator {
    let mut next = migrator();
    let newest = next
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap();
    let mut migrations = next.migrations.to_vec();
    migrations.push(Migration::new(
        newest + 1,
        Cow::Borrowed("next release"),
        MigrationType::Simple,
        Cow::Borrowed("CREATE TABLE IF NOT EXISTS next_release_rows (id BIGINT PRIMARY KEY);"),
        false,
    ));
    next.migrations = Cow::Owned(migrations);
    next
}

/// This release without its newest migration, run the way this release runs it.
fn previous_release() -> Migrator {
    let mut previous = migrator();
    let mut migrations = previous.migrations.to_vec();
    migrations.sort_by_key(|migration| migration.version);
    migrations.pop();
    previous.migrations = Cow::Owned(migrations);
    previous
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn a_release_boots_on_a_schema_a_newer_release_migrated() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL URL");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let name = format!("rollback_{}", uuid::Uuid::new_v4().simple());
    let schema = PostgresSchema::new(&name).unwrap();

    run_in_schema(&pool, &schema, next_release()).await.unwrap();
    let this_release = run_in_schema(&pool, &schema, migrator()).await;
    let previous = run_in_schema(&pool, &schema, previous_release()).await;
    // Going forward again applies nothing new and refuses nothing.
    let forward_again = run_in_schema(&pool, &schema, next_release()).await;

    sqlx::query(&format!("DROP SCHEMA \"{name}\" CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    this_release.expect("this release must boot on a schema the next release migrated");
    previous.expect("the previous release must boot on a schema this release migrated");
    forward_again.unwrap();
}
