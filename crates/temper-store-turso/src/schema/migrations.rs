//! ARN-242: the schema version ledger.

/// Every successful `migrate()` run records the version it brought the
/// database to; boots short-circuit when the ledger already shows the current
/// version. EVERY schema change must bump `SCHEMA_VERSION` in `store/mod.rs`,
/// or stamped databases will skip it.
pub const CREATE_SCHEMA_MIGRATIONS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS temper_schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    applied_at TEXT NOT NULL
)";

/// Highest applied schema version (0 when the ledger is empty).
pub const SELECT_SCHEMA_VERSION: &str =
    "SELECT COALESCE(MAX(version), 0) FROM temper_schema_migrations";

/// Stamp a successfully applied schema version. `INSERT OR IGNORE` makes a
/// concurrent double-stamp a no-op (the version is the primary key).
pub const INSERT_SCHEMA_VERSION: &str = "\
INSERT OR IGNORE INTO temper_schema_migrations (version, name, applied_at) \
VALUES (?1, ?2, datetime('now'))";
