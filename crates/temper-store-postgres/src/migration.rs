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
            include_str!("../migrations/0012_entity_vector_index.sql"),
            include_str!("../migrations/0013_monotonic_vector_reconciliation.sql"),
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
            "entity_key_index",
            "entity_vector_index",
            "entity_vector_index_version",
            "entity_vector_reconciliation_generation",
            "spec_declaration_authority",
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
    fn migration_thirteen_is_tenant_scoped_and_always_withdraws_stale_watermarks() {
        let migration =
            include_str!("../migrations/0013_monotonic_vector_reconciliation.sql").to_lowercase();
        for table in [
            "entity_vector_index_version",
            "entity_vector_reconciliation_generation",
            "spec_declaration_authority",
        ] {
            assert!(
                migration.contains(&format!("alter table {table} enable row level security")),
                "migration 0013 must enable RLS for {table}"
            );
            assert!(
                migration.contains(&format!(
                    "drop policy if exists tenant_isolation on {table}"
                )),
                "migration 0013 tenant policy must be idempotent for {table}"
            );
            assert!(
                migration.contains(&format!("create policy tenant_isolation on {table}")),
                "migration 0013 must create tenant isolation for {table}"
            );
        }

        let authority_trigger = migration
            .split("create or replace function advance_spec_declaration_authority()")
            .nth(1)
            .expect("migration 0013 declaration authority trigger")
            .split("drop trigger if exists specs_declaration_authority_insert")
            .next()
            .expect("migration 0013 declaration authority function body");
        assert!(
            authority_trigger.contains("delete from vector_index_backfill_watermark"),
            "every durable declaration change must withdraw the completion watermark"
        );
        assert!(
            !authority_trigger.contains("if found then"),
            "watermark withdrawal must not depend on an existing generation row"
        );
        assert!(
            migration.contains(
                "add column if not exists declaration_fingerprint text not null default ''"
            ),
            "declaration authority must retain the exact persisted fingerprint"
        );
        assert!(
            migration.contains(
                "select tenant, entity_type, greatest(version::bigint, 1), ioa_source, content_hash, true"
            ),
            "legacy authority seeding must prefer the specs content hash"
        );
        assert!(
            authority_trigger.contains("authority_fingerprint := new.content_hash"),
            "spec triggers must copy the catalog fingerprint into declaration authority"
        );
        assert!(
            authority_trigger.contains("authority_fingerprint := 'absent:v1'"),
            "spec deletion must leave an explicit declaration tombstone fingerprint"
        );
        let update_trigger = migration
            .split("create trigger specs_declaration_authority_update")
            .nth(1)
            .expect("migration 0013 declaration authority update trigger")
            .split("execute function advance_spec_declaration_authority()")
            .next()
            .expect("migration 0013 declaration authority update predicate");
        assert!(
            update_trigger.contains("after update of ioa_source, content_hash, committed on specs"),
            "content-hash-only catalog updates and staged commits must advance authority"
        );
        assert!(
            update_trigger.contains("new.committed is true")
                && update_trigger.contains("old.committed is distinct from true"),
            "only committed declarations, including false-to-true publication, may advance authority"
        );
        assert!(
            migration.contains("when (new.committed is true)")
                && migration.contains("when (old.committed is true)"),
            "staged inserts and deletes must remain invisible to declaration authority"
        );
        assert!(
            authority_trigger.contains("pg_advisory_xact_lock"),
            "spec mutation must serialize with first-writer authority bootstrap"
        );

        let tombstone_function = migration
            .split("create or replace function tombstone_spec_declaration_authority(")
            .nth(1)
            .expect("migration 0013 compatibility-authority tombstone function");
        assert!(
            tombstone_function.contains("delete from specs"),
            "the tombstone entry point must cover persisted catalogs"
        );
        assert!(
            tombstone_function.contains("on conflict (tenant, entity_type) do update set"),
            "the tombstone entry point must cover first-writer authority without a catalog"
        );
        assert!(
            tombstone_function.contains("where spec_declaration_authority.present"),
            "repeating an existing tombstone must be idempotent"
        );
        assert!(
            tombstone_function.contains("delete from vector_index_backfill_watermark"),
            "tombstoning first-writer authority must withdraw completion"
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
