use std::collections::BTreeMap;

use libsql::Connection;
use temper_runtime::persistence::PersistenceError;

use super::catalog::Migration;
use super::runner::{execute_step, schema_object_kind};
use super::schema_snapshot::{IndexColumn, index_columns, normalize_default, type_affinity};
use super::schema_sql::contains_sequence;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct OtsColumnDefinition {
    pub name: &'static str,
    pub affinity: &'static str,
    pub not_null: bool,
    pub default: Option<&'static str>,
    pub primary_key_position: i64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct OtsRebuildDefinition {
    pub algorithm_version: &'static str,
    pub table: &'static str,
    pub temporary_table: &'static str,
    pub required_columns: &'static [OtsColumnDefinition],
    pub updated_at_column: OtsColumnDefinition,
    pub forbidden_table_sql_sequences: &'static [&'static [&'static str]],
    pub schema_tables_query: &'static str,
    pub dependent_objects_query: &'static str,
    pub create_temporary_sql: &'static str,
    pub copy_sql: &'static str,
    pub drop_sql: &'static str,
    pub rename_sql: &'static str,
}

const REQUIRED_COLUMNS: &[OtsColumnDefinition] = &[
    column("trajectory_id", "TEXT", false, None, 1),
    column("tenant", "TEXT", true, None, 0),
    column("agent_id", "TEXT", true, None, 0),
    column("session_id", "TEXT", false, None, 0),
    column("outcome", "TEXT", true, Some("'unknown'"), 0),
    column("entity_type", "TEXT", false, None, 0),
    column("turn_count", "INTEGER", true, Some("0"), 0),
    column("data", "TEXT", true, None, 0),
    column("persistence_status", "TEXT", true, Some("'persisted'"), 0),
    column("persist_attempts", "INTEGER", true, Some("0"), 0),
    column("last_error", "TEXT", false, None, 0),
    column("created_at", "TEXT", true, Some("datetime('now')"), 0),
];

const UPDATED_AT_COLUMN: OtsColumnDefinition =
    column("updated_at", "TEXT", true, Some("datetime('now')"), 0);

const fn column(
    name: &'static str,
    affinity: &'static str,
    not_null: bool,
    default: Option<&'static str>,
    primary_key_position: i64,
) -> OtsColumnDefinition {
    OtsColumnDefinition {
        name,
        affinity,
        not_null,
        default,
        primary_key_position,
    }
}

pub(super) const OTS_REBUILD_DEFINITION: OtsRebuildDefinition = OtsRebuildDefinition {
    algorithm_version: "preserve-dependent-schema-v8-sqlite-identifier-owners",
    table: "ots_trajectories",
    temporary_table: "__temper_migration_ots_trajectories",
    required_columns: REQUIRED_COLUMNS,
    updated_at_column: UPDATED_AT_COLUMN,
    forbidden_table_sql_sequences: &[
        &["CHECK"],
        &["COLLATE"],
        &["GENERATED"],
        &["ON", "CONFLICT"],
        &["AUTOINCREMENT"],
        &["STRICT"],
        &["WITHOUT", "ROWID"],
    ],
    schema_tables_query: "SELECT name FROM sqlite_schema
        WHERE type = 'table' AND name NOT GLOB 'sqlite_*' ORDER BY name",
    dependent_objects_query: "SELECT type, name, sql FROM sqlite_schema
        WHERE tbl_name COLLATE NOCASE = ?1
          AND type IN ('index', 'trigger') AND sql IS NOT NULL
        ORDER BY type, name",
    create_temporary_sql: "CREATE TABLE __temper_migration_ots_trajectories (
        trajectory_id TEXT PRIMARY KEY,
        tenant TEXT NOT NULL,
        agent_id TEXT NOT NULL,
        session_id TEXT,
        outcome TEXT NOT NULL DEFAULT 'unknown',
        entity_type TEXT,
        turn_count INTEGER NOT NULL DEFAULT 0,
        data TEXT NOT NULL,
        persistence_status TEXT NOT NULL DEFAULT 'persisted',
        persist_attempts INTEGER NOT NULL DEFAULT 0,
        last_error TEXT,
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    )",
    copy_sql: "INSERT INTO __temper_migration_ots_trajectories (
        trajectory_id, tenant, agent_id, session_id, outcome, entity_type,
        turn_count, data, persistence_status, persist_attempts, last_error,
        created_at, updated_at
    ) SELECT trajectory_id, tenant, agent_id, session_id, outcome, entity_type,
        turn_count, data, persistence_status, persist_attempts, last_error,
        created_at, COALESCE(created_at, datetime('now'))
    FROM ots_trajectories",
    drop_sql: "DROP TABLE ots_trajectories",
    rename_sql: "ALTER TABLE __temper_migration_ots_trajectories RENAME TO ots_trajectories",
};

#[derive(Debug, Eq, PartialEq)]
struct ObservedColumn {
    affinity: String,
    not_null: bool,
    default: Option<String>,
    primary_key_position: i64,
    hidden: i64,
}

#[derive(Debug)]
struct DependentObject {
    kind: String,
    name: String,
    sql: String,
}

pub(super) async fn rebuild_ots_trajectories(
    connection: &Connection,
    migration: &Migration,
    step_index: usize,
) -> Result<(), PersistenceError> {
    let definition = &OTS_REBUILD_DEFINITION;
    let columns = table_columns(connection, migration, definition.table).await?;
    let already_updated = columns.contains_key(definition.updated_at_column.name);
    validate_columns(migration, definition, &columns, already_updated)?;
    validate_no_table_constraints(connection, migration, definition, !already_updated).await?;
    if already_updated {
        return Ok(());
    }
    let dependent_objects = dependent_objects(connection, migration, definition).await?;

    if schema_object_kind(connection, definition.temporary_table)
        .await?
        .is_some()
    {
        return Err(compatibility_error(
            migration,
            format!(
                "temporary capability '{}' already exists",
                definition.temporary_table
            ),
        ));
    }

    execute_step(
        connection,
        migration,
        step_index,
        definition.create_temporary_sql,
    )
    .await?;
    execute_step(connection, migration, step_index, definition.copy_sql).await?;
    execute_step(connection, migration, step_index, definition.drop_sql).await?;
    execute_step(connection, migration, step_index, definition.rename_sql).await?;
    for object in dependent_objects {
        execute_step(connection, migration, step_index, &object.sql)
            .await
            .map_err(|error| {
                PersistenceError::Storage(format!(
                    "{error}; failed while preserving {} capability '{}'",
                    object.kind, object.name
                ))
            })?;
    }
    Ok(())
}

async fn table_columns(
    connection: &Connection,
    migration: &Migration,
    table: &str,
) -> Result<BTreeMap<String, ObservedColumn>, PersistenceError> {
    let pragma = format!("PRAGMA table_xinfo({})", quote_identifier(table));
    let mut rows = connection
        .query(&pragma, ())
        .await
        .map_err(|error| inspection_error(migration, "inspect OTS columns", error))?;
    let mut columns = BTreeMap::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| inspection_error(migration, "read OTS column", error))?
    {
        let name = row
            .get::<String>(1)
            .map_err(|error| inspection_error(migration, "decode OTS column name", error))?;
        let declared_type = row
            .get::<String>(2)
            .map_err(|error| inspection_error(migration, "decode OTS column type", error))?;
        let default = row
            .get::<Option<String>>(4)
            .map_err(|error| inspection_error(migration, "decode OTS column default", error))?;
        columns.insert(
            name,
            ObservedColumn {
                affinity: type_affinity(&declared_type).to_string(),
                not_null: row.get::<i64>(3).map_err(|error| {
                    inspection_error(migration, "decode OTS not-null flag", error)
                })? != 0,
                default: default.map(|value| normalize_default(&value)),
                primary_key_position: row.get::<i64>(5).map_err(|error| {
                    inspection_error(migration, "decode OTS primary key", error)
                })?,
                hidden: row.get::<i64>(6).map_err(|error| {
                    inspection_error(migration, "decode OTS hidden-column flag", error)
                })?,
            },
        );
    }
    Ok(columns)
}

fn validate_columns(
    migration: &Migration,
    definition: &OtsRebuildDefinition,
    actual: &BTreeMap<String, ObservedColumn>,
    already_updated: bool,
) -> Result<(), PersistenceError> {
    let mut expected = definition.required_columns.to_vec();
    let shape = if already_updated {
        expected.push(definition.updated_at_column);
        "current"
    } else {
        "pre-upgrade"
    };
    if actual.len() != expected.len() {
        let expected_names = expected
            .iter()
            .map(|column| column.name)
            .collect::<Vec<_>>();
        return Err(compatibility_error(
            migration,
            format!(
                "table '{}' must contain exactly the {shape} columns {expected_names:?}; found {:?}",
                definition.table,
                actual.keys().collect::<Vec<_>>()
            ),
        ));
    }
    for expected in expected {
        let Some(observed) = actual.get(expected.name) else {
            return Err(compatibility_error(
                migration,
                format!(
                    "table '{}' is missing required {shape} column '{}'",
                    definition.table, expected.name
                ),
            ));
        };
        let expected_observed = ObservedColumn {
            affinity: expected.affinity.to_string(),
            not_null: expected.not_null,
            default: expected.default.map(str::to_string),
            primary_key_position: expected.primary_key_position,
            hidden: 0,
        };
        if observed != &expected_observed {
            return Err(compatibility_error(
                migration,
                format!(
                    "table '{}' column '{}' has incompatible {shape} semantics: expected {expected_observed:?}, found {observed:?}",
                    definition.table, expected.name
                ),
            ));
        }
    }
    Ok(())
}

async fn validate_no_table_constraints(
    connection: &Connection,
    migration: &Migration,
    definition: &OtsRebuildDefinition,
    reject_inbound_references: bool,
) -> Result<(), PersistenceError> {
    let table = definition.table;
    let mut table_rows = connection
        .query(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [table],
        )
        .await
        .map_err(|error| inspection_error(migration, "inspect OTS table definition", error))?;
    let table_sql = table_rows
        .next()
        .await
        .map_err(|error| inspection_error(migration, "read OTS table definition", error))?
        .ok_or_else(|| compatibility_error(migration, format!("table '{table}' is missing")))?
        .get::<String>(0)
        .map_err(|error| inspection_error(migration, "decode OTS table definition", error))?;
    if let Some(sequence_index) =
        contains_sequence(&table_sql, definition.forbidden_table_sql_sequences)
    {
        let sequence = definition.forbidden_table_sql_sequences[sequence_index].join(" ");
        return Err(compatibility_error(
            migration,
            format!(
                "table '{table}' contains unsupported legacy table semantics matching '{sequence}'"
            ),
        ));
    }
    drop(table_rows);

    let index_pragma = format!("PRAGMA index_list({})", quote_identifier(table));
    let mut indexes = connection
        .query(&index_pragma, ())
        .await
        .map_err(|error| inspection_error(migration, "inspect OTS indexes", error))?;
    while let Some(row) = indexes
        .next()
        .await
        .map_err(|error| inspection_error(migration, "read OTS index", error))?
    {
        let name = row
            .get::<String>(1)
            .map_err(|error| inspection_error(migration, "decode OTS index name", error))?;
        let unique = row
            .get::<i64>(2)
            .map_err(|error| inspection_error(migration, "decode OTS unique flag", error))?
            != 0;
        let origin = row
            .get::<String>(3)
            .map_err(|error| inspection_error(migration, "decode OTS index origin", error))?;
        if unique && origin == "pk" {
            let actual = index_columns(connection, &name).await?;
            let expected = expected_primary_key(definition);
            if actual != expected {
                return Err(compatibility_error(
                    migration,
                    format!(
                        "table '{table}' has incompatible primary key semantics: expected {expected:?}, found {actual:?}"
                    ),
                ));
            }
        } else if unique {
            return Err(compatibility_error(
                migration,
                format!("table '{table}' has an unsupported legacy unique restriction"),
            ));
        }
    }
    drop(indexes);

    let foreign_key_pragma = format!("PRAGMA foreign_key_list({})", quote_identifier(table));
    let mut foreign_keys = connection
        .query(&foreign_key_pragma, ())
        .await
        .map_err(|error| inspection_error(migration, "inspect OTS foreign keys", error))?;
    if foreign_keys
        .next()
        .await
        .map_err(|error| inspection_error(migration, "read OTS foreign key", error))?
        .is_some()
    {
        return Err(compatibility_error(
            migration,
            format!("table '{table}' has unsupported legacy foreign keys"),
        ));
    }
    drop(foreign_keys);

    if !reject_inbound_references {
        return Ok(());
    }

    let mut tables = connection
        .query(definition.schema_tables_query, ())
        .await
        .map_err(|error| inspection_error(migration, "list tables for OTS references", error))?;
    let mut table_names = Vec::new();
    while let Some(row) = tables
        .next()
        .await
        .map_err(|error| inspection_error(migration, "read table for OTS references", error))?
    {
        table_names.push(row.get::<String>(0).map_err(|error| {
            inspection_error(migration, "decode table for OTS references", error)
        })?);
    }
    drop(tables);
    for source_table in table_names {
        if source_table == table {
            continue;
        }
        let pragma = format!(
            "PRAGMA foreign_key_list({})",
            quote_identifier(&source_table)
        );
        let mut references = connection.query(&pragma, ()).await.map_err(|error| {
            inspection_error(migration, "inspect inbound OTS foreign keys", error)
        })?;
        while let Some(row) = references
            .next()
            .await
            .map_err(|error| inspection_error(migration, "read inbound OTS foreign key", error))?
        {
            let target = row.get::<String>(2).map_err(|error| {
                inspection_error(migration, "decode inbound OTS foreign key", error)
            })?;
            if target.eq_ignore_ascii_case(table) {
                return Err(compatibility_error(
                    migration,
                    format!(
                        "table '{source_table}' has an inbound foreign key to legacy table '{table}'"
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn expected_primary_key(definition: &OtsRebuildDefinition) -> Vec<IndexColumn> {
    let mut columns = definition
        .required_columns
        .iter()
        .filter(|column| column.primary_key_position > 0)
        .collect::<Vec<_>>();
    columns.sort_by_key(|column| column.primary_key_position);
    columns
        .into_iter()
        .map(|column| IndexColumn {
            name: Some(column.name.to_string()),
            descending: false,
            collation: Some("binary".to_string()),
        })
        .collect()
}

async fn dependent_objects(
    connection: &Connection,
    migration: &Migration,
    definition: &OtsRebuildDefinition,
) -> Result<Vec<DependentObject>, PersistenceError> {
    let mut rows = connection
        .query(definition.dependent_objects_query, [definition.table])
        .await
        .map_err(|error| inspection_error(migration, "inspect dependent OTS schema", error))?;
    let mut objects = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| inspection_error(migration, "read dependent OTS schema", error))?
    {
        objects.push(DependentObject {
            kind: row.get::<String>(0).map_err(|error| {
                inspection_error(migration, "decode dependent OTS object kind", error)
            })?,
            name: row.get::<String>(1).map_err(|error| {
                inspection_error(migration, "decode dependent OTS object name", error)
            })?,
            sql: row.get::<String>(2).map_err(|error| {
                inspection_error(migration, "decode dependent OTS object SQL", error)
            })?,
        });
    }
    Ok(objects)
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn inspection_error(
    migration: &Migration,
    context: &str,
    error: libsql::Error,
) -> PersistenceError {
    PersistenceError::Storage(format!(
        "Turso migration {} ({}) failed while attempting to {context}: {error} ({error:?})",
        migration.version, migration.name
    ))
}

fn compatibility_error(migration: &Migration, message: String) -> PersistenceError {
    PersistenceError::Storage(format!(
        "Turso migration {} ({}) schema compatibility check failed: {message}",
        migration.version, migration.name
    ))
}
