use std::collections::{BTreeMap, BTreeSet};

use libsql::Connection;
use temper_runtime::persistence::PersistenceError;

use super::schema_sql::{predicate_after_where, restricted_table_semantics};
use super::schema_trigger::{TriggerCapability, capture_triggers};
pub(super) use super::schema_verify::verify_schema;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct ColumnCapability {
    pub affinity: String,
    pub not_null: bool,
    pub default: Option<String>,
    pub primary_key_position: i64,
    pub hidden: i64,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct IndexColumn {
    pub name: Option<String>,
    pub descending: bool,
    pub collation: Option<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct ForeignKeyPart {
    pub id: i64,
    pub sequence: i64,
    pub target_table: String,
    pub source_column: String,
    pub target_column: Option<String>,
    pub on_update: String,
    pub on_delete: String,
    pub match_kind: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct UniqueKeyCapability {
    pub partial: bool,
    pub columns: Vec<IndexColumn>,
    pub predicate: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TableCapability {
    pub columns: BTreeMap<String, ColumnCapability>,
    pub unique_keys: BTreeSet<UniqueKeyCapability>,
    pub foreign_keys: BTreeSet<ForeignKeyPart>,
    pub restricted_semantics: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct IndexCapability {
    pub table: String,
    pub unique: bool,
    pub partial: bool,
    pub columns: Vec<IndexColumn>,
    pub predicate: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SchemaSnapshot {
    pub tables: BTreeMap<String, TableCapability>,
    pub indexes: BTreeMap<String, IndexCapability>,
    pub triggers: BTreeMap<String, TriggerCapability>,
}

impl SchemaSnapshot {
    pub(super) fn manifest(&self) -> String {
        super::schema_manifest::canonical_manifest(self)
    }
}

pub(super) async fn capture_schema(
    connection: &Connection,
) -> Result<SchemaSnapshot, PersistenceError> {
    let table_names = object_names(connection, "table").await?;
    let index_names = named_index_names(connection).await?;

    let mut tables = BTreeMap::new();
    for name in table_names {
        tables.insert(name.clone(), table_capability(connection, &name).await?);
    }

    let mut indexes = BTreeMap::new();
    for name in index_names {
        indexes.insert(name.clone(), index_capability(connection, &name).await?);
    }

    Ok(SchemaSnapshot {
        tables,
        indexes,
        triggers: capture_triggers(connection, None).await?,
    })
}

async fn object_names(
    connection: &Connection,
    object_type: &str,
) -> Result<Vec<String>, PersistenceError> {
    let mut rows = connection
        .query(
            "SELECT name FROM sqlite_schema
             WHERE type = ?1 AND name NOT GLOB 'sqlite_*'
             ORDER BY name",
            [object_type],
        )
        .await
        .map_err(|error| query_error("list schema objects", error))?;
    let mut names = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| query_error("read schema object", error))?
    {
        names.push(
            row.get::<String>(0)
                .map_err(|error| query_error("decode schema object name", error))?,
        );
    }
    Ok(names)
}

async fn named_index_names(connection: &Connection) -> Result<Vec<String>, PersistenceError> {
    let mut rows = connection
        .query(
            "SELECT name FROM sqlite_schema
             WHERE type = 'index' AND sql IS NOT NULL AND name NOT GLOB 'sqlite_*'
             ORDER BY name",
            (),
        )
        .await
        .map_err(|error| query_error("list named indexes", error))?;
    let mut names = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| query_error("read named index", error))?
    {
        names.push(
            row.get::<String>(0)
                .map_err(|error| query_error("decode index name", error))?,
        );
    }
    Ok(names)
}

pub(super) async fn object_kind(
    connection: &Connection,
    name: &str,
) -> Result<Option<String>, PersistenceError> {
    let mut rows = connection
        .query(
            "SELECT type FROM sqlite_schema WHERE name = ?1 ORDER BY type LIMIT 1",
            [name],
        )
        .await
        .map_err(|error| query_error("inspect schema object kind", error))?;
    let row = rows
        .next()
        .await
        .map_err(|error| query_error("read schema object kind", error))?;
    row.map(|row| {
        row.get::<String>(0)
            .map_err(|error| query_error("decode schema object kind", error))
    })
    .transpose()
}

pub(super) async fn table_capability(
    connection: &Connection,
    table: &str,
) -> Result<TableCapability, PersistenceError> {
    let pragma = format!("PRAGMA table_xinfo({})", quote_identifier(table));
    let mut rows = connection
        .query(&pragma, ())
        .await
        .map_err(|error| query_error("inspect table columns", error))?;
    let mut columns = BTreeMap::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| query_error("read table column", error))?
    {
        let name = row
            .get::<String>(1)
            .map_err(|error| query_error("decode column name", error))?;
        let declared_type = row
            .get::<String>(2)
            .map_err(|error| query_error("decode column type", error))?;
        let default = row
            .get::<Option<String>>(4)
            .map_err(|error| query_error("decode column default", error))?;
        columns.insert(
            name,
            ColumnCapability {
                affinity: type_affinity(&declared_type).to_string(),
                not_null: row
                    .get::<i64>(3)
                    .map_err(|error| query_error("decode not-null flag", error))?
                    != 0,
                default: default.map(|value| normalize_default(&value)),
                primary_key_position: row
                    .get::<i64>(5)
                    .map_err(|error| query_error("decode primary-key position", error))?,
                hidden: row
                    .get::<i64>(6)
                    .map_err(|error| query_error("decode hidden-column flag", error))?,
            },
        );
    }
    drop(rows);

    Ok(TableCapability {
        columns,
        unique_keys: unique_keys(connection, table).await?,
        foreign_keys: foreign_keys(connection, table).await?,
        restricted_semantics: restricted_table_semantics(
            &table_definition(connection, table).await?,
        ),
    })
}

async fn table_definition(
    connection: &Connection,
    table: &str,
) -> Result<String, PersistenceError> {
    let mut rows = connection
        .query(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [table],
        )
        .await
        .map_err(|error| query_error("inspect table definition", error))?;
    rows.next()
        .await
        .map_err(|error| query_error("read table definition", error))?
        .ok_or_else(|| compatibility_error(format!("required table '{table}' is missing")))?
        .get::<String>(0)
        .map_err(|error| query_error("decode table definition", error))
}

async fn unique_keys(
    connection: &Connection,
    table: &str,
) -> Result<BTreeSet<UniqueKeyCapability>, PersistenceError> {
    let pragma = format!("PRAGMA index_list({})", quote_identifier(table));
    let mut rows = connection
        .query(&pragma, ())
        .await
        .map_err(|error| query_error("inspect table indexes", error))?;
    let mut indexes = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| query_error("read table index", error))?
    {
        let unique = row
            .get::<i64>(2)
            .map_err(|error| query_error("decode unique-index flag", error))?
            != 0;
        if unique {
            indexes.push((
                row.get::<String>(1)
                    .map_err(|error| query_error("decode unique-index name", error))?,
                row.get::<i64>(4)
                    .map_err(|error| query_error("decode partial-index flag", error))?
                    != 0,
            ));
        }
    }
    drop(rows);

    let mut keys = BTreeSet::new();
    for (name, partial) in indexes {
        let definition = index_definition(connection, &name).await?;
        keys.insert(UniqueKeyCapability {
            partial,
            columns: index_columns(connection, &name).await?,
            predicate: definition.as_deref().and_then(predicate_after_where),
        });
    }
    Ok(keys)
}

async fn index_definition(
    connection: &Connection,
    index: &str,
) -> Result<Option<String>, PersistenceError> {
    let mut rows = connection
        .query(
            "SELECT sql FROM sqlite_schema WHERE type = 'index' AND name = ?1",
            [index],
        )
        .await
        .map_err(|error| query_error("inspect unique-index definition", error))?;
    rows.next()
        .await
        .map_err(|error| query_error("read unique-index definition", error))?
        .ok_or_else(|| compatibility_error(format!("unique index '{index}' is missing")))?
        .get::<Option<String>>(0)
        .map_err(|error| query_error("decode unique-index definition", error))
}

async fn foreign_keys(
    connection: &Connection,
    table: &str,
) -> Result<BTreeSet<ForeignKeyPart>, PersistenceError> {
    let pragma = format!("PRAGMA foreign_key_list({})", quote_identifier(table));
    let mut rows = connection
        .query(&pragma, ())
        .await
        .map_err(|error| query_error("inspect foreign keys", error))?;
    let mut keys = BTreeSet::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| query_error("read foreign key", error))?
    {
        keys.insert(ForeignKeyPart {
            id: row
                .get::<i64>(0)
                .map_err(|error| query_error("decode foreign-key id", error))?,
            sequence: row
                .get::<i64>(1)
                .map_err(|error| query_error("decode foreign-key sequence", error))?,
            target_table: row
                .get::<String>(2)
                .map_err(|error| query_error("decode foreign-key table", error))?,
            source_column: row
                .get::<String>(3)
                .map_err(|error| query_error("decode foreign-key source", error))?,
            target_column: row
                .get::<Option<String>>(4)
                .map_err(|error| query_error("decode foreign-key target", error))?,
            on_update: row
                .get::<String>(5)
                .map_err(|error| query_error("decode foreign-key update action", error))?,
            on_delete: row
                .get::<String>(6)
                .map_err(|error| query_error("decode foreign-key delete action", error))?,
            match_kind: row
                .get::<String>(7)
                .map_err(|error| query_error("decode foreign-key match", error))?,
        });
    }
    Ok(keys)
}

pub(super) async fn index_capability(
    connection: &Connection,
    index: &str,
) -> Result<IndexCapability, PersistenceError> {
    let mut schema_rows = connection
        .query(
            "SELECT tbl_name, sql FROM sqlite_schema WHERE type = 'index' AND name = ?1",
            [index],
        )
        .await
        .map_err(|error| query_error("inspect named index", error))?;
    let schema_row = schema_rows
        .next()
        .await
        .map_err(|error| query_error("read named index", error))?
        .ok_or_else(|| compatibility_error(format!("required index '{index}' is missing")))?;
    let table = schema_row
        .get::<String>(0)
        .map_err(|error| query_error("decode index owner", error))?;
    let sql = schema_row
        .get::<String>(1)
        .map_err(|error| query_error("decode index SQL", error))?;
    drop(schema_rows);

    let pragma = format!("PRAGMA index_list({})", quote_identifier(&table));
    let mut rows = connection
        .query(&pragma, ())
        .await
        .map_err(|error| query_error("inspect index semantics", error))?;
    let mut flags = None;
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| query_error("read index semantics", error))?
    {
        let name = row
            .get::<String>(1)
            .map_err(|error| query_error("decode index name", error))?;
        if name == index {
            flags = Some((
                row.get::<i64>(2)
                    .map_err(|error| query_error("decode unique flag", error))?
                    != 0,
                row.get::<i64>(4)
                    .map_err(|error| query_error("decode partial flag", error))?
                    != 0,
            ));
            break;
        }
    }
    let (unique, partial) = flags.ok_or_else(|| {
        compatibility_error(format!("index '{index}' is not owned by table '{table}'"))
    })?;

    Ok(IndexCapability {
        table,
        unique,
        partial,
        columns: index_columns(connection, index).await?,
        predicate: predicate_after_where(&sql),
    })
}

pub(super) async fn index_columns(
    connection: &Connection,
    index: &str,
) -> Result<Vec<IndexColumn>, PersistenceError> {
    let pragma = format!("PRAGMA index_xinfo({})", quote_identifier(index));
    let mut rows = connection
        .query(&pragma, ())
        .await
        .map_err(|error| query_error("inspect index columns", error))?;
    let mut columns = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| query_error("read index column", error))?
    {
        let key = row
            .get::<i64>(5)
            .map_err(|error| query_error("decode index key flag", error))?;
        if key == 0 {
            continue;
        }
        columns.push(IndexColumn {
            name: row
                .get::<Option<String>>(2)
                .map_err(|error| query_error("decode index column name", error))?,
            descending: row
                .get::<i64>(3)
                .map_err(|error| query_error("decode index sort order", error))?
                != 0,
            collation: row
                .get::<Option<String>>(4)
                .map_err(|error| query_error("decode index collation", error))?
                .map(|value| value.to_ascii_lowercase()),
        });
    }
    Ok(columns)
}

pub(super) fn type_affinity(declared_type: &str) -> &'static str {
    let upper = declared_type.to_ascii_uppercase();
    if upper.contains("INT") {
        "INTEGER"
    } else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
        "TEXT"
    } else if upper.contains("BLOB") || upper.is_empty() {
        "BLOB"
    } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        "REAL"
    } else {
        "NUMERIC"
    }
}

pub(super) fn normalize_default(value: &str) -> String {
    let mut normalized = value.trim();
    while normalized.starts_with('(') && normalized.ends_with(')') {
        normalized = normalized[1..normalized.len() - 1].trim();
    }
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn query_error(context: &str, error: libsql::Error) -> PersistenceError {
    PersistenceError::Storage(format!(
        "Turso schema introspection failed while attempting to {context}: {error} ({error:?})"
    ))
}

pub(super) fn compatibility_error(message: String) -> PersistenceError {
    PersistenceError::Storage(format!(
        "Turso schema compatibility check failed: {message}"
    ))
}
