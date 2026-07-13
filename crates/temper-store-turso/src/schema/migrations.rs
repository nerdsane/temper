//! ARN-242: the schema version ledger.

/// Every successful `migrate()` run records the version it brought the
/// database to; boots short-circuit when the ledger already shows the current
/// version. EVERY schema change must bump `SCHEMA_VERSION` in `store/mod.rs`,
/// or stamped databases will skip it.
pub const CREATE_SCHEMA_MIGRATIONS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS temper_schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    fingerprint TEXT NOT NULL DEFAULT '',
    applied_at TEXT NOT NULL
)";

/// The ledger's OWN migration. The ledger table sits BEFORE the gate that
/// governs everything else, so it can never be gated by its own fingerprint —
/// it must migrate itself, un-gated, on every boot. Without this, adding any
/// column to the ledger (`fingerprint` here; `applied_by`, `duration_ms`,
/// anything later) makes the very next statement — the gate SELECT — fail at
/// prepare time with "no such column" on every database whose ledger predates
/// it: a hard boot failure, and precisely the class this ADR exists to kill.
///
/// Routed through `execute_idempotent`, so on a fresh database (where
/// `CREATE TABLE` already declared the column) it is a tolerated
/// duplicate-column no-op that leaves `sqlite_master` — and therefore
/// `SCHEMA_FINGERPRINT` — unchanged.
pub const ALTER_SCHEMA_MIGRATIONS_ADD_FINGERPRINT: &str = "\
ALTER TABLE temper_schema_migrations ADD COLUMN fingerprint TEXT NOT NULL DEFAULT ''";

/// Has this database EVER been migrated to the declared schema? This — not
/// the version — is the boot gate: the DDL is skipped only when a ledger row
/// records the fingerprint the binary declares, so a schema change cannot skip
/// existing databases even if the author forgets to bump `SCHEMA_VERSION`.
///
/// Asking "ever migrated to this schema" rather than "is the LATEST row this
/// schema" also makes a rollback cheap: a binary rolled back to an older
/// schema finds its own retained row and skips, instead of re-running the
/// whole DDL on every boot for as long as the rollback lasts. (Caveat: when
/// the newer build changed the schema WITHOUT bumping the version — the
/// version is only a label — `INSERT OR REPLACE` overwrote the older row, so
/// the rolled-back binary re-runs the DDL once and then skips.)
///
/// It compares the STORED declared constant against the CURRENT declared
/// constant — never the live schema — so a platform database's extra
/// `migrate_platform` tables cannot cause spurious re-runs.
pub const SELECT_SCHEMA_FINGERPRINT_APPLIED: &str = "\
SELECT EXISTS(SELECT 1 FROM temper_schema_migrations WHERE fingerprint = ?1)";

/// Stamp a successfully applied schema version and its fingerprint.
/// `INSERT OR REPLACE` on the version primary key makes a concurrent
/// double-stamp idempotent, and lets a same-version DDL change (an author who
/// updated the fingerprint without bumping the version) record its new
/// fingerprint after the DDL actually ran.
pub const INSERT_SCHEMA_VERSION: &str = "\
INSERT OR REPLACE INTO temper_schema_migrations \
(version, name, fingerprint, applied_at) \
VALUES (?1, ?2, ?3, datetime('now'))";
