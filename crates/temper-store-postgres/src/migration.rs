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
    sqlx::query(schema::CREATE_EVENTS_TABLE)
        .execute(pool)
        .await
        .map_err(|e| PersistenceError::Storage(format!("failed to create events table: {e}")))?;

    sqlx::query(schema::CREATE_SNAPSHOTS_TABLE)
        .execute(pool)
        .await
        .map_err(|e| PersistenceError::Storage(format!("failed to create snapshots table: {e}")))?;

    sqlx::query(schema::CREATE_SPECS_TABLE)
        .execute(pool)
        .await
        .map_err(|e| PersistenceError::Storage(format!("failed to create specs table: {e}")))?;

    sqlx::query(schema::CREATE_TRAJECTORIES_TABLE)
        .execute(pool)
        .await
        .map_err(|e| {
            PersistenceError::Storage(format!("failed to create trajectories table: {e}"))
        })?;

    sqlx::query(schema::CREATE_TRAJECTORIES_SUCCESS_INDEX)
        .execute(pool)
        .await
        .map_err(|e| {
            PersistenceError::Storage(format!("failed to create trajectories success index: {e}"))
        })?;

    sqlx::query(schema::CREATE_TRAJECTORIES_ENTITY_INDEX)
        .execute(pool)
        .await
        .map_err(|e| {
            PersistenceError::Storage(format!("failed to create trajectories entity index: {e}"))
        })?;

    sqlx::query(schema::CREATE_DESIGN_TIME_EVENTS_TABLE)
        .execute(pool)
        .await
        .map_err(|e| {
            PersistenceError::Storage(format!("failed to create design_time_events table: {e}"))
        })?;

    sqlx::query(schema::CREATE_DESIGN_TIME_EVENTS_TENANT_INDEX)
        .execute(pool)
        .await
        .map_err(|e| {
            PersistenceError::Storage(format!(
                "failed to create design_time_events tenant index: {e}"
            ))
        })?;

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
    }
}
