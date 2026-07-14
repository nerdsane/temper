use std::collections::BTreeSet;

use libsql::{Builder, Connection, TransactionBehavior, params};
use temper_runtime::persistence::PersistenceError;

use super::catalog::{MIGRATIONS, Migration, MigrationStep};
use super::ledger::{CREATE_MIGRATION_LEDGER, validate_ledger_schema};
use super::ots_rebuild::rebuild_ots_trajectories;
use super::schema_snapshot::{SchemaSnapshot, capture_schema, verify_schema};

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct FaultInjection {
    pub after_step: Option<(u32, usize)>,
    pub ddl_error_at: Option<(u32, usize)>,
}

#[derive(Debug)]
struct LedgerRow {
    version: u32,
    name: String,
    checksum: String,
}

#[derive(Debug)]
struct ExpectedMigration {
    snapshot: SchemaSnapshot,
    checksum: String,
}

pub(crate) async fn migrate(connection: &Connection) -> Result<(), PersistenceError> {
    run_migrations(connection, MIGRATIONS, FaultInjection::default()).await
}

#[cfg(test)]
pub(super) async fn migrate_prefix(
    connection: &Connection,
    migration_count: usize,
    fault: FaultInjection,
) -> Result<(), PersistenceError> {
    assert!(migration_count <= MIGRATIONS.len());
    run_migrations(connection, &MIGRATIONS[..migration_count], fault).await
}

async fn run_migrations(
    connection: &Connection,
    catalog: &[Migration],
    fault: FaultInjection,
) -> Result<(), PersistenceError> {
    validate_catalog(catalog)?;
    let expected_migrations = build_expected_migrations(catalog).await?;
    ensure_ledger(connection).await?;
    validate_ledger_rows(
        &load_ledger(connection).await?,
        catalog,
        &expected_migrations,
    )?;
    for (index, migration) in catalog.iter().enumerate() {
        apply_migration(
            connection,
            catalog,
            migration,
            &expected_migrations,
            index,
            fault,
        )
        .await?;
    }

    let final_ledger = load_ledger(connection).await?;
    validate_ledger_rows(&final_ledger, catalog, &expected_migrations)?;
    require_ledger_length(&final_ledger, catalog.len(), "after migration run")?;
    for (migration, expected) in catalog.iter().zip(&expected_migrations) {
        verify_schema(connection, &expected.snapshot)
            .await
            .map_err(|error| migration_context(migration, "verify catalog schema", error))?;
    }
    Ok(())
}

async fn ensure_ledger(connection: &Connection) -> Result<(), PersistenceError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(|error| migration_sql_error("begin migration-ledger transaction", error))?;
    let outcome = async {
        transaction
            .execute(CREATE_MIGRATION_LEDGER, ())
            .await
            .map_err(|error| migration_sql_error("create migration ledger", error))?;
        validate_ledger_schema(&transaction).await
    }
    .await;
    finish_transaction(transaction, outcome, "create migration ledger").await
}

async fn apply_migration(
    connection: &Connection,
    catalog: &[Migration],
    migration: &Migration,
    expected_migrations: &[ExpectedMigration],
    migration_index: usize,
    fault: FaultInjection,
) -> Result<(), PersistenceError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .await
        .map_err(|error| {
            migration_sql_error(
                &format!("begin migration {} ({})", migration.version, migration.name),
                error,
            )
        })?;

    let outcome = async {
        let ledger = load_ledger(&transaction).await?;
        validate_ledger_rows(&ledger, catalog, expected_migrations)?;
        if ledger
            .last()
            .is_some_and(|row| row.version >= migration.version)
        {
            return Ok(());
        }

        for (step_index, step) in migration.steps.iter().enumerate() {
            if fault.ddl_error_at == Some((migration.version, step_index)) {
                execute_step(
                    &transaction,
                    migration,
                    step_index,
                    "ALTER TABLE __temper_injected_missing_table ADD COLUMN value TEXT",
                )
                .await?;
            }
            apply_step(&transaction, migration, step_index, step).await?;
            if fault.after_step == Some((migration.version, step_index)) {
                return Err(PersistenceError::Storage(format!(
                    "injected migration interruption after version {} step {step_index}",
                    migration.version
                )));
            }
        }

        let expected = &expected_migrations[migration_index];
        verify_schema(&transaction, &expected.snapshot)
            .await
            .map_err(|error| migration_context(migration, "verify schema", error))?;
        let inserted = transaction
            .execute(
                "INSERT INTO temper_schema_migrations (version, name, checksum)
                 VALUES (?1, ?2, ?3)",
                params![
                    migration.version as i64,
                    migration.name,
                    expected.checksum.as_str()
                ],
            )
            .await
            .map_err(|error| {
                migration_sql_error(
                    &format!(
                        "record migration {} ({})",
                        migration.version, migration.name
                    ),
                    error,
                )
            })?;
        if inserted != 1 {
            return Err(PersistenceError::Storage(format!(
                "Turso migration ledger insert for version {} ({}) affected {inserted} rows; expected 1",
                migration.version, migration.name
            )));
        }
        let retained_ledger = load_ledger(&transaction).await?;
        validate_ledger_rows(&retained_ledger, catalog, expected_migrations)?;
        require_ledger_length(
            &retained_ledger,
            migration_index + 1,
            &format!("after recording migration {}", migration.version),
        )?;
        Ok(())
    }
    .await;

    finish_transaction(
        transaction,
        outcome,
        &format!("apply migration {} ({})", migration.version, migration.name),
    )
    .await
}

async fn finish_transaction(
    transaction: libsql::Transaction,
    outcome: Result<(), PersistenceError>,
    context: &str,
) -> Result<(), PersistenceError> {
    match outcome {
        Ok(()) => transaction
            .commit()
            .await
            .map_err(|error| migration_sql_error(&format!("commit {context}"), error)),
        Err(error) => match transaction.rollback().await {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(PersistenceError::Storage(format!(
                "{error}; rollback also failed while attempting to {context}: {rollback_error} ({rollback_error:?})"
            ))),
        },
    }
}

async fn apply_step(
    connection: &Connection,
    migration: &Migration,
    step_index: usize,
    step: &MigrationStep,
) -> Result<(), PersistenceError> {
    match step {
        MigrationStep::Sql(sql) => execute_step(connection, migration, step_index, sql).await,
        MigrationStep::AddColumn { table, column, sql } => {
            require_table_kind(connection, migration, step_index, table).await?;
            if column_exists(connection, table, column).await? {
                Ok(())
            } else {
                execute_step(connection, migration, step_index, sql).await
            }
        }
        MigrationStep::RebuildOtsTrajectories => {
            rebuild_ots_trajectories(connection, migration, step_index).await
        }
    }
}

pub(super) async fn execute_step(
    connection: &Connection,
    migration: &Migration,
    step_index: usize,
    sql: &str,
) -> Result<(), PersistenceError> {
    connection
        .execute(sql, ())
        .await
        .map(|_| ())
        .map_err(|error| {
            migration_sql_error(
                &format!(
                    "apply migration {} ({}) step {step_index}: {}",
                    migration.version,
                    migration.name,
                    sql.split_whitespace().take(8).collect::<Vec<_>>().join(" ")
                ),
                error,
            )
        })
}

async fn column_exists(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, PersistenceError> {
    Ok(table_columns(connection, table).await?.contains(column))
}

async fn require_table_kind(
    connection: &Connection,
    migration: &Migration,
    step_index: usize,
    table: &str,
) -> Result<(), PersistenceError> {
    let kind = schema_object_kind(connection, table).await?;
    if kind.as_deref() == Some("table") {
        return Ok(());
    }
    Err(PersistenceError::Storage(format!(
        "Turso migration {} ({}) step {step_index} schema compatibility check failed: capability '{table}' must be a table, found {}",
        migration.version,
        migration.name,
        kind.as_deref().unwrap_or("no schema object")
    )))
}

pub(super) async fn table_columns(
    connection: &Connection,
    table: &str,
) -> Result<BTreeSet<String>, PersistenceError> {
    let pragma = format!("PRAGMA table_xinfo({})", quote_identifier(table));
    let mut rows = connection
        .query(&pragma, ())
        .await
        .map_err(|error| migration_sql_error(&format!("inspect table '{table}'"), error))?;
    let mut columns = BTreeSet::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| migration_sql_error(&format!("read table '{table}'"), error))?
    {
        columns.insert(row.get::<String>(1).map_err(|error| {
            migration_sql_error(&format!("decode column for table '{table}'"), error)
        })?);
    }
    Ok(columns)
}

pub(super) async fn schema_object_kind(
    connection: &Connection,
    name: &str,
) -> Result<Option<String>, PersistenceError> {
    let mut rows = connection
        .query(
            "SELECT type FROM sqlite_schema WHERE name = ?1 ORDER BY type LIMIT 1",
            [name],
        )
        .await
        .map_err(|error| migration_sql_error("inspect schema object", error))?;
    rows.next()
        .await
        .map_err(|error| migration_sql_error("read schema object", error))?
        .map(|row| {
            row.get::<String>(0)
                .map_err(|error| migration_sql_error("decode schema object", error))
        })
        .transpose()
}

async fn build_expected_migrations(
    catalog: &[Migration],
) -> Result<Vec<ExpectedMigration>, PersistenceError> {
    let database = Builder::new_local(":memory:")
        .build()
        .await
        .map_err(|error| migration_sql_error("build reference schema database", error))?;
    let connection = database
        .connect()
        .map_err(|error| migration_sql_error("connect to reference schema database", error))?;
    let mut expected_migrations = Vec::with_capacity(catalog.len());
    for migration in catalog {
        for (step_index, step) in migration.steps.iter().enumerate() {
            apply_step(&connection, migration, step_index, step).await?;
        }
        let snapshot = capture_schema(&connection).await?;
        let checksum = migration.checksum(&snapshot.manifest());
        expected_migrations.push(ExpectedMigration { snapshot, checksum });
    }
    Ok(expected_migrations)
}

#[cfg(test)]
pub(super) async fn expected_checksums() -> Result<Vec<String>, PersistenceError> {
    Ok(build_expected_migrations(MIGRATIONS)
        .await?
        .into_iter()
        .map(|migration| migration.checksum)
        .collect())
}

async fn load_ledger(connection: &Connection) -> Result<Vec<LedgerRow>, PersistenceError> {
    let mut rows = connection
        .query(
            "SELECT version, name, checksum FROM temper_schema_migrations ORDER BY version",
            (),
        )
        .await
        .map_err(|error| migration_sql_error("load migration ledger", error))?;
    let mut ledger = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| migration_sql_error("read migration-ledger row", error))?
    {
        let version = row
            .get::<i64>(0)
            .map_err(|error| migration_sql_error("decode migration version", error))?;
        let version = u32::try_from(version).map_err(|_| {
            PersistenceError::Storage(format!(
                "Turso migration ledger contains invalid version {version}"
            ))
        })?;
        ledger.push(LedgerRow {
            version,
            name: row
                .get::<String>(1)
                .map_err(|error| migration_sql_error("decode migration name", error))?,
            checksum: row
                .get::<String>(2)
                .map_err(|error| migration_sql_error("decode migration checksum", error))?,
        });
    }
    Ok(ledger)
}

fn validate_catalog(catalog: &[Migration]) -> Result<(), PersistenceError> {
    for (index, migration) in catalog.iter().enumerate() {
        let expected = (index + 1) as u32;
        if migration.version != expected {
            return Err(PersistenceError::Storage(format!(
                "Turso migration catalog is not contiguous: expected version {expected}, found {}",
                migration.version
            )));
        }
    }
    Ok(())
}

fn validate_ledger_rows(
    ledger: &[LedgerRow],
    catalog: &[Migration],
    expected_migrations: &[ExpectedMigration],
) -> Result<(), PersistenceError> {
    for (index, row) in ledger.iter().enumerate() {
        let expected_version = (index + 1) as u32;
        if row.version != expected_version {
            return Err(PersistenceError::Storage(format!(
                "Turso migration ledger has a version gap: expected {expected_version}, found {}",
                row.version
            )));
        }
        let Some(migration) = catalog.get(index) else {
            return Err(PersistenceError::Storage(format!(
                "Turso database schema version {} is newer than this binary supports (latest supported version {})",
                row.version,
                catalog.last().map_or(0, |migration| migration.version)
            )));
        };
        if row.name != migration.name {
            return Err(PersistenceError::Storage(format!(
                "Turso migration ledger name mismatch at version {}: expected '{}', found '{}'",
                row.version, migration.name, row.name
            )));
        }
        let expected_checksum = &expected_migrations[index].checksum;
        if &row.checksum != expected_checksum {
            return Err(PersistenceError::Storage(format!(
                "Turso migration ledger checksum mismatch at version {} ({}): expected {}, found {}",
                row.version, row.name, expected_checksum, row.checksum
            )));
        }
    }
    Ok(())
}

fn require_ledger_length(
    ledger: &[LedgerRow],
    expected: usize,
    context: &str,
) -> Result<(), PersistenceError> {
    if ledger.len() == expected {
        return Ok(());
    }
    Err(PersistenceError::Storage(format!(
        "Turso migration ledger is incomplete {context}: expected {expected} rows, found {}",
        ledger.len()
    )))
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn migration_sql_error(context: &str, error: libsql::Error) -> PersistenceError {
    PersistenceError::Storage(format!(
        "Turso schema migration failed while attempting to {context}: {error} ({error:?})"
    ))
}

fn migration_context(
    migration: &Migration,
    context: &str,
    error: PersistenceError,
) -> PersistenceError {
    PersistenceError::Storage(format!(
        "Turso migration {} ({}) failed while attempting to {context}: {error}",
        migration.version, migration.name
    ))
}
