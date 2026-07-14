use libsql::Connection;
use temper_runtime::persistence::PersistenceError;

use super::schema_snapshot::{
    IndexCapability, SchemaSnapshot, TableCapability, compatibility_error, index_capability,
    object_kind, table_capability,
};

pub(super) const EXTRA_COLUMN_POLICY: &str =
    "allow-visible-nullable-no-default-non-primary-key-non-rowid-shadow-v2";

pub(super) async fn verify_schema(
    connection: &Connection,
    expected: &SchemaSnapshot,
) -> Result<(), PersistenceError> {
    for (table_name, expected_table) in &expected.tables {
        let kind = object_kind(connection, table_name).await?;
        if kind.as_deref() != Some("table") {
            return Err(compatibility_error(format!(
                "capability '{table_name}' must be a table, found {}",
                kind.as_deref().unwrap_or("no schema object")
            )));
        }
        let actual = table_capability(connection, table_name).await?;
        verify_table(table_name, expected_table, &actual)?;
    }

    for (index_name, expected_index) in &expected.indexes {
        verify_index(connection, index_name, expected_index).await?;
    }
    Ok(())
}

async fn verify_index(
    connection: &Connection,
    name: &str,
    expected: &IndexCapability,
) -> Result<(), PersistenceError> {
    let kind = object_kind(connection, name).await?;
    if kind.as_deref() != Some("index") {
        return Err(compatibility_error(format!(
            "capability '{name}' must be an index, found {}",
            kind.as_deref().unwrap_or("no schema object")
        )));
    }
    let actual = index_capability(connection, name).await?;
    if &actual != expected {
        return Err(compatibility_error(format!(
            "index '{name}' has incompatible semantics: expected {expected:?}, found {actual:?}"
        )));
    }
    Ok(())
}

fn verify_table(
    name: &str,
    expected: &TableCapability,
    actual: &TableCapability,
) -> Result<(), PersistenceError> {
    for (column_name, expected_column) in &expected.columns {
        let Some(actual_column) = actual.columns.get(column_name) else {
            return Err(compatibility_error(format!(
                "table '{name}' is missing required column '{column_name}'"
            )));
        };
        if actual_column != expected_column {
            return Err(compatibility_error(format!(
                "table '{name}' column '{column_name}' has incompatible semantics: expected {expected_column:?}, found {actual_column:?}"
            )));
        }
    }

    for (column_name, column) in &actual.columns {
        if expected.columns.contains_key(column_name) {
            continue;
        }
        let shadows_rowid = matches!(
            column_name.to_ascii_lowercase().as_str(),
            "rowid" | "_rowid_" | "oid"
        );
        if column.not_null
            || column.default.is_some()
            || column.primary_key_position != 0
            || column.hidden != 0
            || shadows_rowid
        {
            return Err(compatibility_error(format!(
                "table '{name}' has unexpected required column or omission-unsafe extension '{column_name}' with semantics {column:?}"
            )));
        }
    }

    if actual.unique_keys != expected.unique_keys {
        return Err(compatibility_error(format!(
            "table '{name}' has incompatible unique key restrictions: expected {:?}, found {:?}",
            expected.unique_keys, actual.unique_keys
        )));
    }
    if actual.foreign_keys != expected.foreign_keys {
        return Err(compatibility_error(format!(
            "table '{name}' has incompatible foreign key restrictions: expected {:?}, found {:?}",
            expected.foreign_keys, actual.foreign_keys
        )));
    }
    if actual.restricted_semantics != expected.restricted_semantics {
        return Err(compatibility_error(format!(
            "table '{name}' has incompatible restricted table semantics: expected {:?}, found {:?}",
            expected.restricted_semantics, actual.restricted_semantics
        )));
    }
    Ok(())
}
