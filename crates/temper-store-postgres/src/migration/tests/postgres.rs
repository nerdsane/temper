use std::borrow::Cow;
use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool};

use super::super::{CONVERGENCE_MIGRATOR, FORK_MIGRATOR, UPSTREAM_MIGRATOR, run_migrations};

#[derive(Clone, Copy, Debug)]
enum TestHistory {
    Fresh,
    CommonPrefix,
    ForkPartial,
    ForkComplete,
    UpstreamPartial,
    UpstreamComplete,
    AlreadyConverged,
}

#[derive(Clone, Copy, Debug)]
enum InvalidHistory {
    DivergentWithoutCommon,
    DivergentAfterPartialCommon,
    Mixed,
    UnknownChecksum,
    Gapped,
    Failed,
}

fn subset_migrator(
    source: &'static sqlx::migrate::Migrator,
    versions: std::ops::RangeInclusive<i64>,
) -> sqlx::migrate::Migrator {
    sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            source
                .iter()
                .filter(|migration| versions.contains(&migration.version))
                .cloned()
                .collect(),
        ),
        ignore_missing: true,
        locking: true,
        no_tx: false,
    }
}

async fn seed_history(pool: &PgPool, history: TestHistory) {
    let common = subset_migrator(&FORK_MIGRATOR, 1..=11);
    match history {
        TestHistory::Fresh => {}
        TestHistory::CommonPrefix => common.run(pool).await.unwrap(),
        TestHistory::ForkPartial => {
            common.run(pool).await.unwrap();
            subset_migrator(&FORK_MIGRATOR, 12..=12)
                .run(pool)
                .await
                .unwrap();
        }
        TestHistory::ForkComplete => FORK_MIGRATOR.run(pool).await.unwrap(),
        TestHistory::UpstreamPartial => {
            common.run(pool).await.unwrap();
            subset_migrator(&UPSTREAM_MIGRATOR, 12..=13)
                .run(pool)
                .await
                .unwrap();
        }
        TestHistory::UpstreamComplete => {
            common.run(pool).await.unwrap();
            UPSTREAM_MIGRATOR.run(pool).await.unwrap();
        }
        TestHistory::AlreadyConverged => {
            FORK_MIGRATOR.run(pool).await.unwrap();
            CONVERGENCE_MIGRATOR.run(pool).await.unwrap();
        }
    }
}

async fn seed_invalid_history(pool: &PgPool, history: InvalidHistory) {
    let common = subset_migrator(&FORK_MIGRATOR, 1..=11);
    common.run(pool).await.unwrap();
    match history {
        InvalidHistory::DivergentWithoutCommon => {
            subset_migrator(&UPSTREAM_MIGRATOR, 12..=12)
                .run(pool)
                .await
                .unwrap();
            sqlx::query("DELETE FROM _sqlx_migrations WHERE version <= 11")
                .execute(pool)
                .await
                .unwrap();
        }
        InvalidHistory::DivergentAfterPartialCommon => {
            subset_migrator(&FORK_MIGRATOR, 12..=12)
                .run(pool)
                .await
                .unwrap();
            sqlx::query("DELETE FROM _sqlx_migrations WHERE version BETWEEN 6 AND 11")
                .execute(pool)
                .await
                .unwrap();
        }
        InvalidHistory::Mixed => {
            subset_migrator(&FORK_MIGRATOR, 12..=12)
                .run(pool)
                .await
                .unwrap();
            subset_migrator(&UPSTREAM_MIGRATOR, 13..=13)
                .run(pool)
                .await
                .unwrap();
        }
        InvalidHistory::UnknownChecksum => {
            subset_migrator(&FORK_MIGRATOR, 12..=12)
                .run(pool)
                .await
                .unwrap();
            sqlx::query("UPDATE _sqlx_migrations SET checksum = $1 WHERE version = 12")
                .bind(vec![0_u8; 48])
                .execute(pool)
                .await
                .unwrap();
        }
        InvalidHistory::Gapped => {
            sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 6")
                .execute(pool)
                .await
                .unwrap();
        }
        InvalidHistory::Failed => {
            sqlx::query("UPDATE _sqlx_migrations SET success = FALSE WHERE version = 11")
                .execute(pool)
                .await
                .unwrap();
        }
    }
}

async fn migration_history(pool: &PgPool, last_version: i64) -> Vec<(i64, Vec<u8>, bool)> {
    let table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(pool)
            .await
            .unwrap();
    if !table_exists {
        return Vec::new();
    }
    sqlx::query_as::<_, (i64, Vec<u8>, bool)>(
        "SELECT version, checksum, success FROM _sqlx_migrations WHERE version <= $1 ORDER BY version",
    )
    .bind(last_version)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn assert_union_schema(pool: &PgPool) {
    for table in [
        "entity_vector_index",
        "schema_deployments",
        "feature_requests",
        "evolution_records",
        "trajectories",
        "ots_trajectories",
    ] {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(table)
            .fetch_one(pool)
            .await
            .unwrap();
        assert!(exists, "converged schema is missing table {table}");
    }
    for (table, column) in [
        ("feature_requests", "tenant"),
        ("evolution_records", "tenant"),
        ("trajectories", "capture_seq"),
    ] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM information_schema.columns
                WHERE table_schema = current_schema()
                  AND table_name = $1
                  AND column_name = $2
            )",
        )
        .bind(table)
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap();
        assert!(exists, "converged schema is missing {table}.{column}");
    }
    let tenant_key_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_constraint
            WHERE conname = 'ots_trajectories_tenant_identity'
              AND conrelid = 'ots_trajectories'::regclass
        )",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert!(tenant_key_exists, "converged OTS tenant key is missing");
}

async fn create_test_database(
    admin_pool: &PgPool,
    options: &PgConnectOptions,
    label: &str,
) -> (String, PgPool) {
    let database_name = format!("temper_migration_{label}_{}", uuid::Uuid::new_v4().simple());
    admin_pool
        .execute(format!("CREATE DATABASE \"{database_name}\"").as_str())
        .await
        .unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone().database(&database_name))
        .await
        .unwrap();
    (database_name, pool)
}

async fn drop_test_database(admin_pool: &PgPool, database_name: &str, pool: PgPool) {
    pool.close().await;
    admin_pool
        .execute(format!("DROP DATABASE \"{database_name}\" WITH (FORCE)").as_str())
        .await
        .unwrap();
}

#[tokio::test]
async fn real_postgres_converges_every_supported_history_without_rewriting_it() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        assert_ne!(
            std::env::var("TEMPER_REQUIRE_BACKEND_PARITY").as_deref(),
            Ok("1"),
            "DATABASE_URL is required by the backend parity CI gate"
        );
        return;
    };
    let options = PgConnectOptions::from_str(&database_url).unwrap();
    let admin_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone())
        .await
        .unwrap();

    for history in [
        TestHistory::Fresh,
        TestHistory::CommonPrefix,
        TestHistory::ForkPartial,
        TestHistory::ForkComplete,
        TestHistory::UpstreamPartial,
        TestHistory::UpstreamComplete,
        TestHistory::AlreadyConverged,
    ] {
        let label = format!("{history:?}").to_lowercase();
        let (database_name, pool) = create_test_database(&admin_pool, &options, &label).await;
        seed_history(&pool, history).await;
        let original_history = migration_history(&pool, 15).await;

        run_migrations(&pool).await.unwrap();
        let migrated_history = migration_history(&pool, 15).await;
        for original in &original_history {
            assert!(
                migrated_history.contains(original),
                "migration runner rewrote existing history {original:?} for {history:?}"
            );
        }
        assert_eq!(migration_history(&pool, 16).await.last().unwrap().0, 16);
        assert_union_schema(&pool).await;

        let converged_history = migration_history(&pool, i64::MAX).await;
        run_migrations(&pool).await.unwrap();
        assert_eq!(
            migration_history(&pool, i64::MAX).await,
            converged_history,
            "second startup must not rewrite migration history for {history:?}"
        );
        drop_test_database(&admin_pool, &database_name, pool).await;
    }

    for (history, expected_error) in [
        (
            InvalidHistory::DivergentWithoutCommon,
            "before the common stream is complete",
        ),
        (
            InvalidHistory::DivergentAfterPartialCommon,
            "before the common stream is complete",
        ),
        (InvalidHistory::Mixed, "unexpected checksum"),
        (InvalidHistory::UnknownChecksum, "unknown lineage checksum"),
        (InvalidHistory::Gapped, "gap at version 6"),
        (InvalidHistory::Failed, "failed version 11"),
    ] {
        let label = format!("invalid_{history:?}").to_lowercase();
        let (database_name, pool) = create_test_database(&admin_pool, &options, &label).await;
        seed_invalid_history(&pool, history).await;
        let original_history = migration_history(&pool, i64::MAX).await;

        let error = run_migrations(&pool).await.unwrap_err().to_string();
        assert!(error.contains(expected_error), "{history:?}: {error}");
        assert_eq!(
            migration_history(&pool, i64::MAX).await,
            original_history,
            "invalid history must fail before mutation for {history:?}"
        );
        assert!(
            original_history
                .iter()
                .all(|(version, _, _)| *version != 16),
            "invalid fixture unexpectedly contains convergence migration"
        );
        drop_test_database(&admin_pool, &database_name, pool).await;
    }
    admin_pool.close().await;
}
