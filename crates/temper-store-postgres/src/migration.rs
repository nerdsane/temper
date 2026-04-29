//! Versioned schema migration runner.
//!
//! Postgres is the canonical schema source. The migration files under
//! `crates/temper-store-postgres/migrations/` are executed through
//! `sqlx::migrate!()` so production cutovers can reason about schema version,
//! not just a bag of startup-time `CREATE TABLE IF NOT EXISTS` statements.

use sqlx::PgPool;
use temper_runtime::persistence::PersistenceError;

/// Run all schema migrations.
///
/// Creates or upgrades all persistence tables used by Temper. The initial
/// migration remains idempotent because existing local/dev databases may have
/// been created by the pre-ADR-0065 bootstrap runner before `_sqlx_migrations`
/// existed.
pub async fn run_migrations(pool: &PgPool) -> Result<(), PersistenceError> {
    static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
    MIGRATOR
        .run(pool)
        .await
        .map_err(|e| PersistenceError::Storage(format!("failed to run Postgres migrations: {e}")))
}

#[cfg(test)]
mod tests {
    use crate::schema;

    #[test]
    fn versioned_migration_is_the_schema_source() {
        let migration = include_str!("../migrations/0001_initial.sql").to_lowercase();
        for table in [
            "events",
            "snapshots",
            "specs",
            "trajectories",
            "entity_catalog",
            "entity_field_index",
            "tenant_secrets",
            "blobs",
        ] {
            assert!(
                migration.contains(&format!("create table if not exists {table}")),
                "versioned migration missing table: {table}"
            );
        }
        assert!(
            migration.contains("enable row level security"),
            "versioned migration must carry tenant RLS setup"
        );
    }

    #[test]
    fn migration_sql_is_idempotent() {
        // Both schemas must use IF NOT EXISTS so repeated execution is safe.
        assert!(
            schema::CREATE_EVENTS_TABLE.contains("IF NOT EXISTS"),
            "events DDL must be idempotent"
        );
        assert!(
            schema::CREATE_SNAPSHOTS_TABLE.contains("IF NOT EXISTS"),
            "snapshots DDL must be idempotent"
        );
        assert!(
            schema::CREATE_SPECS_TABLE.contains("IF NOT EXISTS"),
            "specs DDL must be idempotent"
        );
        assert!(
            schema::CREATE_TRAJECTORIES_TABLE.contains("IF NOT EXISTS"),
            "trajectories DDL must be idempotent"
        );
        assert!(
            schema::CREATE_DESIGN_TIME_EVENTS_TABLE.contains("IF NOT EXISTS"),
            "design_time_events DDL must be idempotent"
        );
        assert!(
            schema::CREATE_TENANT_CONSTRAINTS_TABLE.contains("IF NOT EXISTS"),
            "tenant_constraints DDL must be idempotent"
        );
        assert!(
            schema::CREATE_ENTITY_LISTING_INDEX.contains("IF NOT EXISTS"),
            "entity listing index DDL must be idempotent"
        );
        assert!(
            schema::CREATE_WASM_MODULES_TABLE.contains("IF NOT EXISTS"),
            "wasm_modules DDL must be idempotent"
        );
        assert!(
            schema::CREATE_WASM_INVOCATION_LOGS_TABLE.contains("IF NOT EXISTS"),
            "wasm_invocation_logs DDL must be idempotent"
        );
        assert!(
            schema::CREATE_PENDING_DECISIONS_TABLE.contains("IF NOT EXISTS"),
            "pending_decisions DDL must be idempotent"
        );
        assert!(
            schema::CREATE_ENTITY_CATALOG_TABLE.contains("IF NOT EXISTS"),
            "entity_catalog DDL must be idempotent"
        );
    }
}
