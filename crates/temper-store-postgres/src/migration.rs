//! Versioned schema migration runner.
//!
//! Postgres is the canonical schema source. The migration files under
//! `crates/temper-store-postgres/migrations/` preserve the fork lineage,
//! `migrations-upstream/` preserve the original upstream divergence,
//! `migrations-upstream-fixed/` preserve upstream's corrected stream,
//! `migrations-convergence/` preserve the fork's historical `0016`, and
//! `migrations-shared/` contain the shared sequence beginning at `0017`.
//! ADR-0173 defines the fail-closed classifier that selects a legacy stream
//! before applying the shared sequence.

use sqlx::migrate::{Migrate, Migrator};
use sqlx::{PgConnection, PgPool, Row};
use temper_runtime::persistence::PersistenceError;

mod lineage;

#[cfg(test)]
use lineage::migration_at;
use lineage::{AppliedMigrationRow, MigrationLineage, classify_migration_lineage};

static FORK_MIGRATOR: Migrator = {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.ignore_missing = true;
    migrator
};

static LEGACY_UPSTREAM_MIGRATOR: Migrator = {
    let mut migrator = sqlx::migrate!("./migrations-upstream");
    migrator.ignore_missing = true;
    migrator
};

static FIXED_UPSTREAM_MIGRATOR: Migrator = {
    let mut migrator = sqlx::migrate!("./migrations-upstream-fixed");
    migrator.ignore_missing = true;
    migrator
};

static HISTORICAL_CONVERGENCE_MIGRATOR: Migrator = {
    let mut migrator = sqlx::migrate!("./migrations-convergence");
    migrator.ignore_missing = true;
    migrator
};

static SHARED_MIGRATOR: Migrator = {
    let mut migrator = sqlx::migrate!("./migrations-shared");
    migrator.ignore_missing = true;
    migrator
};

/// Run all schema migrations.
///
/// Creates or upgrades all persistence tables used by Temper. The initial
/// migration remains idempotent because existing local/dev databases may have
/// been created by the pre-ADR-0065 bootstrap runner before `_sqlx_migrations`
/// existed.
pub async fn run_migrations(pool: &PgPool) -> Result<(), PersistenceError> {
    let mut connection = pool.acquire().await.map_err(|error| {
        PersistenceError::Storage(format!(
            "failed to acquire Postgres migration connection: {error}"
        ))
    })?;
    // A failed nested migrator can retain a re-entrant advisory lock. Closing
    // this dedicated connection on drop guarantees every lock is released.
    connection.close_on_drop();
    Migrate::lock(&mut *connection)
        .await
        .map_err(migration_error)?;

    let result = run_migrations_locked(&mut connection).await;
    let unlock_result = Migrate::unlock(&mut *connection)
        .await
        .map_err(migration_error);
    match (result, unlock_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn run_migrations_locked(connection: &mut PgConnection) -> Result<(), PersistenceError> {
    let applied = load_applied_migrations(connection).await?;
    let lineage = classify_migration_lineage(&applied).map_err(PersistenceError::Storage)?;

    match lineage {
        MigrationLineage::Fork => FORK_MIGRATOR
            .run_direct(connection)
            .await
            .map_err(migration_error)?,
        MigrationLineage::LegacyUpstream => LEGACY_UPSTREAM_MIGRATOR
            .run_direct(connection)
            .await
            .map_err(migration_error)?,
        MigrationLineage::FixedUpstream => FIXED_UPSTREAM_MIGRATOR
            .run_direct(connection)
            .await
            .map_err(migration_error)?,
    }
    if lineage != MigrationLineage::FixedUpstream {
        HISTORICAL_CONVERGENCE_MIGRATOR
            .run_direct(connection)
            .await
            .map_err(migration_error)?;
    }
    SHARED_MIGRATOR
        .run_direct(connection)
        .await
        .map_err(migration_error)
}

async fn load_applied_migrations(
    connection: &mut PgConnection,
) -> Result<Vec<AppliedMigrationRow>, PersistenceError> {
    let table_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| {
                PersistenceError::Storage(format!(
                    "failed to inspect Postgres migration history: {error}"
                ))
            })?;
    if !table_exists {
        return Ok(Vec::new());
    }

    sqlx::query("SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version")
        .fetch_all(&mut *connection)
        .await
        .map_err(|error| {
            PersistenceError::Storage(format!(
                "failed to read Postgres migration history: {error}"
            ))
        })?
        .into_iter()
        .map(|row| {
            Ok(AppliedMigrationRow {
                version: row.try_get("version").map_err(history_decode_error)?,
                checksum: row.try_get("checksum").map_err(history_decode_error)?,
                success: row.try_get("success").map_err(history_decode_error)?,
            })
        })
        .collect()
}

fn history_decode_error(error: sqlx::Error) -> PersistenceError {
    PersistenceError::Storage(format!(
        "failed to decode Postgres migration history: {error}"
    ))
}

fn migration_error(error: sqlx::migrate::MigrateError) -> PersistenceError {
    PersistenceError::Storage(format!("failed to run Postgres migrations: {error}"))
}

#[cfg(test)]
#[path = "migration/tests.rs"]
mod tests;
