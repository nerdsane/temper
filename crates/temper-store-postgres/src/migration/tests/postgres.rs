use std::borrow::Cow;
use std::str::FromStr;

use sqlx::migrate::Migration;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor, PgPool};

use super::super::{
    FIXED_UPSTREAM_MIGRATOR, FORK_MIGRATOR, HISTORICAL_CONVERGENCE_MIGRATOR,
    LEGACY_UPSTREAM_MIGRATOR, SHARED_MIGRATOR, run_migrations,
};

#[derive(Clone, Copy, Debug)]
enum LineageFixture {
    Fork,
    LegacyUpstream,
    FixedUpstream,
}

#[derive(Clone, Copy, Debug)]
enum InvalidHistory {
    DivergentWithoutCommon,
    DivergentAfterPartialCommon,
    Mixed,
    WrongSixteen,
    SharedBeforeComplete,
    UnknownChecksum,
    Gapped,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SchemaSnapshot {
    columns: Vec<(String, String, String, String, Option<String>)>,
    indexes: Vec<(String, String, String)>,
    constraints: Vec<(String, String, String)>,
}

fn migrations_in_range(
    source: &'static sqlx::migrate::Migrator,
    versions: std::ops::RangeInclusive<i64>,
) -> Vec<&'static Migration> {
    source
        .iter()
        .filter(|migration| versions.contains(&migration.version))
        .collect()
}

fn lineage_boundaries(lineage: LineageFixture) -> Vec<&'static Migration> {
    let mut migrations = migrations_in_range(&FORK_MIGRATOR, 1..=11);
    match lineage {
        LineageFixture::Fork => {
            migrations.extend(migrations_in_range(&FORK_MIGRATOR, 12..=13));
            migrations.extend(migrations_in_range(
                &HISTORICAL_CONVERGENCE_MIGRATOR,
                16..=16,
            ));
        }
        LineageFixture::LegacyUpstream => {
            migrations.extend(migrations_in_range(&LEGACY_UPSTREAM_MIGRATOR, 12..=15));
            migrations.extend(migrations_in_range(
                &HISTORICAL_CONVERGENCE_MIGRATOR,
                16..=16,
            ));
        }
        LineageFixture::FixedUpstream => {
            migrations.extend(migrations_in_range(&FORK_MIGRATOR, 12..=12));
            migrations.extend(migrations_in_range(&FIXED_UPSTREAM_MIGRATOR, 13..=16));
        }
    }
    migrations.extend(migrations_in_range(&SHARED_MIGRATOR, 17..=i64::MAX));
    migrations
}

fn subset_migrator(migrations: &[&Migration]) -> sqlx::migrate::Migrator {
    sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            migrations
                .iter()
                .map(|migration| (*migration).clone())
                .collect(),
        ),
        ignore_missing: true,
        locking: true,
        no_tx: false,
    }
}

async fn seed_boundaries(pool: &PgPool, migrations: &[&Migration]) {
    for migration in migrations {
        subset_migrator(&[*migration]).run(pool).await.unwrap();
    }
}

async fn seed_invalid_history(pool: &PgPool, history: InvalidHistory) {
    let common = migrations_in_range(&FORK_MIGRATOR, 1..=11);
    seed_boundaries(pool, &common).await;
    match history {
        InvalidHistory::DivergentWithoutCommon => {
            seed_boundaries(
                pool,
                &migrations_in_range(&LEGACY_UPSTREAM_MIGRATOR, 12..=12),
            )
            .await;
            sqlx::query("DELETE FROM _sqlx_migrations WHERE version <= 11")
                .execute(pool)
                .await
                .unwrap();
        }
        InvalidHistory::DivergentAfterPartialCommon => {
            seed_boundaries(pool, &migrations_in_range(&FORK_MIGRATOR, 12..=12)).await;
            sqlx::query("DELETE FROM _sqlx_migrations WHERE version BETWEEN 6 AND 11")
                .execute(pool)
                .await
                .unwrap();
        }
        InvalidHistory::Mixed => {
            seed_boundaries(
                pool,
                &migrations_in_range(&LEGACY_UPSTREAM_MIGRATOR, 12..=12),
            )
            .await;
            seed_boundaries(pool, &migrations_in_range(&FORK_MIGRATOR, 13..=13)).await;
        }
        InvalidHistory::WrongSixteen => {
            seed_boundaries(pool, &migrations_in_range(&FORK_MIGRATOR, 12..=13)).await;
            seed_boundaries(
                pool,
                &migrations_in_range(&FIXED_UPSTREAM_MIGRATOR, 16..=16),
            )
            .await;
        }
        InvalidHistory::SharedBeforeComplete => {
            seed_boundaries(pool, &migrations_in_range(&FORK_MIGRATOR, 12..=12)).await;
            seed_boundaries(pool, &migrations_in_range(&SHARED_MIGRATOR, 17..=17)).await;
        }
        InvalidHistory::UnknownChecksum => {
            seed_boundaries(pool, &migrations_in_range(&FORK_MIGRATOR, 12..=12)).await;
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

async fn migration_history(pool: &PgPool) -> Vec<(i64, Vec<u8>, bool)> {
    let table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(pool)
            .await
            .unwrap();
    if !table_exists {
        return Vec::new();
    }
    sqlx::query_as::<_, (i64, Vec<u8>, bool)>(
        "SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn schema_snapshot(pool: &PgPool) -> SchemaSnapshot {
    let columns = sqlx::query_as::<_, (String, String, String, String, Option<String>)>(
        "SELECT table_name, column_name, data_type, is_nullable, column_default
         FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name <> '_sqlx_migrations'
         ORDER BY table_name, ordinal_position",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let indexes = sqlx::query_as::<_, (String, String, String)>(
        "SELECT tablename, indexname, indexdef
         FROM pg_indexes
         WHERE schemaname = current_schema()
           AND tablename <> '_sqlx_migrations'
         ORDER BY tablename, indexname",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let constraints = sqlx::query_as::<_, (String, String, String)>(
        "SELECT relation.relname, c.conname,
                pg_get_constraintdef(c.oid)
         FROM pg_constraint AS c
         JOIN pg_class AS relation ON relation.oid = c.conrelid
         JOIN pg_namespace AS namespace ON namespace.oid = relation.relnamespace
         WHERE namespace.nspname = current_schema()
           AND relation.relname <> '_sqlx_migrations'
         ORDER BY relation.relname, c.conname",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    SchemaSnapshot {
        columns,
        indexes,
        constraints,
    }
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
    let pool = connect_database(options, &database_name).await;
    (database_name, pool)
}

async fn connect_database(options: &PgConnectOptions, database_name: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.clone().database(database_name))
        .await
        .unwrap()
}

async fn drop_test_database(admin_pool: &PgPool, database_name: &str, pool: PgPool) {
    pool.close().await;
    admin_pool
        .execute(format!("DROP DATABASE \"{database_name}\" WITH (FORCE)").as_str())
        .await
        .unwrap();
}

#[tokio::test]
async fn real_postgres_restarts_after_every_boundary_and_converges_identically() {
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
    let mut expected_schema = None;

    for lineage in [
        LineageFixture::Fork,
        LineageFixture::LegacyUpstream,
        LineageFixture::FixedUpstream,
    ] {
        let boundaries = lineage_boundaries(lineage);
        for boundary in 0..=boundaries.len() {
            let label = format!("{lineage:?}_{boundary}").to_lowercase();
            let (database_name, seed_pool) =
                create_test_database(&admin_pool, &options, &label).await;
            seed_boundaries(&seed_pool, &boundaries[..boundary]).await;
            let original_history = migration_history(&seed_pool).await;
            seed_pool.close().await;

            let pool = connect_database(&options, &database_name).await;
            run_migrations(&pool).await.unwrap();
            let migrated_history = migration_history(&pool).await;
            for original in &original_history {
                assert!(
                    migrated_history.contains(original),
                    "migration runner rewrote existing history {original:?} for \
                     {lineage:?} boundary {boundary}"
                );
            }
            assert_eq!(
                migrated_history.last().map(|row| row.0),
                Some(18),
                "{lineage:?} boundary {boundary} did not reach the shared stream"
            );
            assert_union_schema(&pool).await;
            let snapshot = schema_snapshot(&pool).await;
            if let Some(expected) = &expected_schema {
                assert_eq!(
                    &snapshot, expected,
                    "{lineage:?} boundary {boundary} converged to a different schema"
                );
            } else {
                expected_schema = Some(snapshot);
            }

            let converged_history = migrated_history;
            pool.close().await;
            let restarted_pool = connect_database(&options, &database_name).await;
            run_migrations(&restarted_pool).await.unwrap();
            assert_eq!(
                migration_history(&restarted_pool).await,
                converged_history,
                "second restart rewrote migration history for {lineage:?} boundary {boundary}"
            );
            drop_test_database(&admin_pool, &database_name, restarted_pool).await;
        }
    }
    admin_pool.close().await;
}

#[tokio::test]
async fn real_postgres_rejects_invalid_histories_before_mutation() {
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
        (InvalidHistory::WrongSixteen, "unexpected checksum"),
        (InvalidHistory::SharedBeforeComplete, "stream is complete"),
        (InvalidHistory::UnknownChecksum, "unknown lineage checksum"),
        (InvalidHistory::Gapped, "gap at version 6"),
        (InvalidHistory::Failed, "failed version 11"),
    ] {
        let label = format!("invalid_{history:?}").to_lowercase();
        let (database_name, pool) = create_test_database(&admin_pool, &options, &label).await;
        seed_invalid_history(&pool, history).await;
        let original_history = migration_history(&pool).await;

        let error = run_migrations(&pool).await.unwrap_err().to_string();
        assert!(error.contains(expected_error), "{history:?}: {error}");
        assert_eq!(
            migration_history(&pool).await,
            original_history,
            "invalid history must fail before mutation for {history:?}"
        );
        drop_test_database(&admin_pool, &database_name, pool).await;
    }
    admin_pool.close().await;
}
