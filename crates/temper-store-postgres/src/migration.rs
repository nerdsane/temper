//! Lightweight schema migration runner.
//!
//! Executes the `CREATE TABLE IF NOT EXISTS` statements defined in
//! [`crate::schema`] against the provided connection pool.  This is
//! intentionally simple — for production systems consider a full migration
//! framework such as `sqlx migrate` or `refinery`.

use sqlx::PgPool;
use temper_runtime::persistence::PersistenceError;

use crate::schema;

/// Run all schema migrations.
///
/// Creates all persistence tables used by Temper if they do not already exist.
/// The statements are idempotent so this function is safe to call on every
/// application start-up.
pub async fn run_migrations(pool: &PgPool) -> Result<(), PersistenceError> {
    let statements = [
        ("events table", schema::CREATE_EVENTS_TABLE),
        ("snapshots table", schema::CREATE_SNAPSHOTS_TABLE),
        ("specs table", schema::CREATE_SPECS_TABLE),
        (
            "specs content_hash migration",
            schema::ALTER_SPECS_ADD_CONTENT_HASH,
        ),
        (
            "specs committed migration",
            schema::ALTER_SPECS_ADD_COMMITTED,
        ),
        ("trajectories table", schema::CREATE_TRAJECTORIES_TABLE),
        (
            "trajectories success index",
            schema::CREATE_TRAJECTORIES_SUCCESS_INDEX,
        ),
        (
            "trajectories entity index",
            schema::CREATE_TRAJECTORIES_ENTITY_INDEX,
        ),
        (
            "design_time_events table",
            schema::CREATE_DESIGN_TIME_EVENTS_TABLE,
        ),
        (
            "design_time_events tenant index",
            schema::CREATE_DESIGN_TIME_EVENTS_TENANT_INDEX,
        ),
        (
            "tenant_constraints table",
            schema::CREATE_TENANT_CONSTRAINTS_TABLE,
        ),
        ("entity listing index", schema::CREATE_ENTITY_LISTING_INDEX),
        ("wasm_modules table", schema::CREATE_WASM_MODULES_TABLE),
        (
            "wasm_invocation_logs table",
            schema::CREATE_WASM_INVOCATION_LOGS_TABLE,
        ),
        (
            "wasm_invocation_logs tenant index",
            schema::CREATE_WASM_INVOCATION_LOGS_TENANT_INDEX,
        ),
        (
            "wasm_invocation_logs module index",
            schema::CREATE_WASM_INVOCATION_LOGS_MODULE_INDEX,
        ),
        (
            "wasm_invocation_logs created index",
            schema::CREATE_WASM_INVOCATION_LOGS_CREATED_INDEX,
        ),
        ("tenant_secrets table", schema::CREATE_TENANT_SECRETS_TABLE),
        (
            "pending_decisions table",
            schema::CREATE_PENDING_DECISIONS_TABLE,
        ),
        (
            "pending_decisions tenant index",
            schema::CREATE_PENDING_DECISIONS_TENANT_INDEX,
        ),
        (
            "pending_decisions status index",
            schema::CREATE_PENDING_DECISIONS_STATUS_INDEX,
        ),
        (
            "tenant_policies table",
            schema::CREATE_TENANT_POLICIES_TABLE,
        ),
        ("policies table", schema::CREATE_POLICIES_TABLE),
        (
            "policy_denial_patterns table",
            schema::CREATE_POLICY_DENIAL_PATTERNS_TABLE,
        ),
        (
            "policy_denial_patterns tenant index",
            schema::CREATE_POLICY_DENIAL_PATTERNS_TENANT_INDEX,
        ),
        (
            "tenant_installed_apps table",
            schema::CREATE_TENANT_INSTALLED_APPS_TABLE,
        ),
        ("entity_catalog table", schema::CREATE_ENTITY_CATALOG_TABLE),
        (
            "entity_catalog type index",
            schema::CREATE_ENTITY_CATALOG_TYPE_INDEX,
        ),
        (
            "entity_catalog status index",
            schema::CREATE_ENTITY_CATALOG_STATUS_INDEX,
        ),
        (
            "entity_catalog fields gin index",
            schema::CREATE_ENTITY_CATALOG_FIELDS_GIN_INDEX,
        ),
        (
            "entity_field_index table",
            schema::CREATE_ENTITY_FIELD_INDEX_TABLE,
        ),
        (
            "entity_field_index lookup index",
            schema::CREATE_ENTITY_FIELD_INDEX_LOOKUP,
        ),
        (
            "entity_field_index status index",
            schema::CREATE_ENTITY_FIELD_INDEX_STATUS,
        ),
        (
            "feature_requests table",
            schema::CREATE_FEATURE_REQUESTS_TABLE,
        ),
        (
            "evolution_records table",
            schema::CREATE_EVOLUTION_RECORDS_TABLE,
        ),
        (
            "evolution_records tenant migration",
            schema::ALTER_EVOLUTION_RECORDS_ADD_TENANT,
        ),
        (
            "evolution_records type/status index",
            schema::CREATE_EVOLUTION_RECORDS_TYPE_STATUS_INDEX,
        ),
        (
            "evolution_records derived_from index",
            schema::CREATE_EVOLUTION_RECORDS_DERIVED_FROM_INDEX,
        ),
        (
            "ots_trajectories table",
            schema::CREATE_OTS_TRAJECTORIES_TABLE,
        ),
        (
            "ots_trajectories agent index",
            schema::CREATE_OTS_TRAJECTORIES_AGENT_INDEX,
        ),
        (
            "ots_trajectories tenant index",
            schema::CREATE_OTS_TRAJECTORIES_TENANT_INDEX,
        ),
        (
            "ots_trajectories outcome index",
            schema::CREATE_OTS_TRAJECTORIES_OUTCOME_INDEX,
        ),
        ("blobs table", schema::CREATE_BLOBS_TABLE),
        (
            "blobs expires_at index",
            schema::CREATE_BLOBS_EXPIRES_AT_INDEX,
        ),
    ];

    for (label, sql) in statements {
        sqlx::query(sql)
            .execute(pool)
            .await
            .map_err(|e| PersistenceError::Storage(format!("failed to create {label}: {e}")))?;
    }

    // Enable row-level security on all tenant-scoped tables.
    for stmt in schema::ENABLE_TENANT_RLS {
        sqlx::query(stmt)
            .execute(pool)
            .await
            .map_err(|e| PersistenceError::Storage(format!("failed to enable tenant RLS: {e}")))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::schema;

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
