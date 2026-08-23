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
        let migration = [
            include_str!("../migrations/0001_initial.sql"),
            include_str!("../migrations/0002_wasm_modules_source.sql"),
            include_str!("../migrations/0003_published_artifacts.sql"),
            include_str!("../migrations/0004_entity_catalog_state.sql"),
            include_str!("../migrations/0005_installed_app_genesis_provenance.sql"),
            include_str!("../migrations/0006_segmented_event_history.sql"),
            include_str!("../migrations/0007_installed_app_follow_policy.sql"),
            include_str!("../migrations/0008_ots_trajectory_outbox_status.sql"),
            include_str!("../migrations/0009_entity_key_index.sql"),
            include_str!("../migrations/0010_key_index_backfill_watermark.sql"),
            include_str!("../migrations/0011_key_index_watermark_key_set.sql"),
            include_str!("../migrations/0016_evolution_tenant_ownership.sql"),
        ]
        .join("\n")
        .to_lowercase();
        for table in [
            "events",
            "snapshots",
            "specs",
            "trajectories",
            "entity_catalog",
            "entity_field_index",
            "tenant_secrets",
            "blobs",
            "published_artifacts",
            "event_segments",
            "snapshot_history",
            "ots_trajectories",
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
        assert!(
            migration.contains("segment_index"),
            "versioned migration must add event segment metadata"
        );
        assert!(
            migration.contains("add column if not exists state jsonb"),
            "versioned migration must add entity catalog state metadata"
        );
        assert!(
            migration.contains("persistence_status"),
            "versioned migration must add OTS outbox status metadata"
        );
    }

    #[test]
    fn migration_four_keeps_historical_entity_catalog_state() {
        let migration_four = include_str!("../migrations/0004_entity_catalog_state.sql");
        assert!(
            migration_four.contains("ADD COLUMN IF NOT EXISTS state JSONB"),
            "migration 0004 is already applied in production and must stay entity_catalog_state"
        );
        assert!(
            !migration_four.contains("event_segments"),
            "segmented event history must not reuse migration version 0004"
        );
    }

    #[test]
    fn migration_six_keeps_historical_segmented_event_history() {
        let migration_six = include_str!("../migrations/0006_segmented_event_history.sql");
        assert!(
            migration_six.contains("CREATE TABLE IF NOT EXISTS event_segments"),
            "migration 0006 is already applied in production and must stay segmented_event_history"
        );
        assert!(
            !migration_six.contains("entity_catalog"),
            "entity_catalog state must not reuse migration version 0006"
        );
    }

    #[test]
    fn capture_order_column_exists_on_both_schema_paths() {
        // A fresh bootstrap reads schema.rs and an existing database reads the
        // migration; a session read that orders by `capture_seq` fails on
        // whichever path forgets it.
        let migration = include_str!("../migrations/0014_trajectory_capture_seq.sql");
        assert!(
            migration.contains("ADD COLUMN IF NOT EXISTS capture_seq"),
            "migration 0014 must add the capture-order column idempotently"
        );
        assert!(
            migration.contains("idx_trajectories_session_capture"),
            "migration 0014 must cover the session read's ordering columns"
        );
        assert!(
            schema::CREATE_TRAJECTORIES_TABLE.contains("capture_seq"),
            "a freshly bootstrapped trajectories table must carry the capture-order column"
        );
    }

    #[test]
    fn ots_trajectory_identity_is_tenant_scoped_on_both_schema_paths() {
        // A fresh bootstrap reads schema.rs and an existing database reads the
        // migration. Whichever path keeps the global key lets one tenant's
        // upload overwrite another tenant's trajectory.
        let migration = include_str!("../migrations/0015_ots_trajectory_tenant_identity.sql");
        assert!(
            migration.contains("DROP CONSTRAINT IF EXISTS ots_trajectories_pkey"),
            "migration 0015 must drop the globally keyed primary key idempotently"
        );
        assert!(
            migration.contains("PRIMARY KEY (tenant, trajectory_id)"),
            "migration 0015 must rekey the table by (tenant, trajectory_id)"
        );
        assert!(
            schema::CREATE_OTS_TRAJECTORIES_TABLE.contains("PRIMARY KEY (tenant, trajectory_id)"),
            "a freshly bootstrapped ots_trajectories table must be keyed by tenant and id"
        );
        assert!(
            !schema::CREATE_OTS_TRAJECTORIES_TABLE
                .contains("trajectory_id TEXT         PRIMARY KEY"),
            "the globally keyed column definition must be gone, not shadowed"
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
            schema::CREATE_EVENTS_TABLE.contains("segment_index"),
            "events rows must carry segment metadata"
        );
        assert!(
            schema::ALTER_EVENTS_ADD_SEGMENT_INDEX.contains("ADD COLUMN IF NOT EXISTS"),
            "events segment migration must be idempotent"
        );
        assert!(
            schema::CREATE_EVENT_SEGMENTS_TABLE.contains("IF NOT EXISTS"),
            "event segment DDL must be idempotent"
        );
        assert!(
            schema::CREATE_SNAPSHOT_HISTORY_TABLE.contains("IF NOT EXISTS"),
            "snapshot history DDL must be idempotent"
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
        assert!(
            schema::CREATE_PUBLISHED_ARTIFACTS_TABLE.contains("IF NOT EXISTS"),
            "published_artifacts DDL must be idempotent"
        );
    }
}

#[cfg(test)]
mod version_uniqueness {
    /// Two files sharing a version prefix both embed via `sqlx::migrate!`,
    /// and every persistent database then fails checksum validation on boot
    /// no matter which content it has applied (observed in production with
    /// duplicate 0012 files). Guard the invariant at the source.
    #[test]
    fn migration_versions_are_unique() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
        let mut versions = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(dir).expect("migrations dir") {
            let name = entry.expect("dir entry").file_name();
            let name = name.to_string_lossy().into_owned();
            if !name.ends_with(".sql") {
                continue;
            }
            let prefix: String = name.chars().take_while(|c| c.is_ascii_digit()).collect();
            assert!(!prefix.is_empty(), "migration lacks numeric prefix: {name}");
            if let Some(previous) = versions.insert(prefix.clone(), name.clone()) {
                panic!("duplicate migration version {prefix}: {previous} and {name}");
            }
        }
    }
}
