use libsql::Connection;
use temper_runtime::persistence::PersistenceError;

use super::schema_sql::normalize_schema_ddl;

pub(super) const CREATE_MIGRATION_LEDGER: &str = "\
CREATE TABLE IF NOT EXISTS temper_schema_migrations (
    version INTEGER PRIMARY KEY CHECK (version > 0),
    name TEXT NOT NULL UNIQUE,
    checksum TEXT NOT NULL CHECK (length(checksum) = 64),
    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

pub(super) async fn validate_ledger_schema(
    connection: &Connection,
) -> Result<(), PersistenceError> {
    let mut rows = connection
        .query(
            "SELECT type, sql FROM sqlite_schema
             WHERE name = 'temper_schema_migrations' ORDER BY type LIMIT 1",
            (),
        )
        .await
        .map_err(|error| ledger_error("inspect migration-ledger schema", error))?;
    let row = rows
        .next()
        .await
        .map_err(|error| ledger_error("read migration-ledger schema", error))?
        .ok_or_else(|| {
            PersistenceError::Storage("Turso migration ledger table is missing".to_string())
        })?;
    let kind = row
        .get::<String>(0)
        .map_err(|error| ledger_error("decode migration-ledger object kind", error))?;
    if kind != "table" {
        return Err(PersistenceError::Storage(format!(
            "Turso migration ledger capability must be a table, found {kind}"
        )));
    }
    let actual = row
        .get::<String>(1)
        .map_err(|error| ledger_error("decode migration-ledger schema", error))?;
    drop(rows);

    if normalize_schema_ddl(&actual) != normalize_schema_ddl(CREATE_MIGRATION_LEDGER) {
        return Err(PersistenceError::Storage(format!(
            "Turso migration ledger has incompatible schema: expected {}, found {}",
            normalize_schema_ddl(CREATE_MIGRATION_LEDGER),
            normalize_schema_ddl(&actual)
        )));
    }

    let mut triggers = connection
        .query(
            "SELECT name FROM sqlite_schema
             WHERE type = 'trigger' AND tbl_name = 'temper_schema_migrations'
             ORDER BY name LIMIT 1",
            (),
        )
        .await
        .map_err(|error| ledger_error("inspect migration-ledger triggers", error))?;
    if let Some(row) = triggers
        .next()
        .await
        .map_err(|error| ledger_error("read migration-ledger trigger", error))?
    {
        let name = row
            .get::<String>(0)
            .map_err(|error| ledger_error("decode migration-ledger trigger", error))?;
        return Err(PersistenceError::Storage(format!(
            "Turso migration ledger has unsupported trigger '{name}'"
        )));
    }
    Ok(())
}

fn ledger_error(context: &str, error: libsql::Error) -> PersistenceError {
    PersistenceError::Storage(format!(
        "Turso schema migration failed while attempting to {context}: {error} ({error:?})"
    ))
}
