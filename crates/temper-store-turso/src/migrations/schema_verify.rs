use libsql::Connection;
use temper_runtime::persistence::PersistenceError;

use super::schema_ots_probe::validate_legacy_ots_triggers;
use super::schema_snapshot::{
    IndexCapability, SchemaSnapshot, TableCapability, compatibility_error, index_capability,
    object_kind, table_capability,
};
use super::schema_trigger::capture_triggers;

pub(super) const EXTRA_COLUMN_POLICY: &str =
    "allow-visible-nullable-no-default-non-primary-key-non-rowid-shadow-v2";
pub(super) const EXTRA_INDEX_POLICY: &str =
    "allow-nonunique-full-plain-column-index-with-builtin-collation-v1";
pub(super) const TRIGGER_POLICY: &str = concat!(
    "exact-trigger-set-with-sqlite-identifier-owners-and-",
    "parsed-audit-sink-contract-with-transaction-pinned-production-upsert-probe-v7"
);

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
        verify_triggers(connection, table_name, expected).await?;
    }

    for (index_name, expected_index) in &expected.indexes {
        verify_index(connection, index_name, expected_index).await?;
    }
    for table_name in expected.tables.keys() {
        verify_index_extensions(connection, table_name, expected).await?;
    }
    Ok(())
}

async fn verify_triggers(
    connection: &Connection,
    table: &str,
    expected: &SchemaSnapshot,
) -> Result<(), PersistenceError> {
    let actual = capture_triggers(connection, Some(table)).await?;
    let mut unexpected = Vec::new();
    for (name, actual_trigger) in &actual {
        match expected.triggers.get(name) {
            Some(expected_trigger) if expected_trigger == actual_trigger => {}
            Some(expected_trigger) => {
                return Err(compatibility_error(format!(
                    "trigger '{name}' has incompatible semantics: expected {expected_trigger:?}, found {actual_trigger:?}"
                )));
            }
            None => {
                unexpected.push((name.as_str(), actual_trigger));
            }
        }
    }
    if !unexpected.is_empty() {
        if table == "ots_trajectories" {
            validate_legacy_ots_triggers(connection, &unexpected).await?;
        } else {
            return Err(compatibility_error(format!(
                "table '{table}' has unexpected executable trigger '{}'",
                unexpected[0].0
            )));
        }
    }
    for (name, expected_trigger) in &expected.triggers {
        if expected_trigger.table.eq_ignore_ascii_case(table) && !actual.contains_key(name) {
            return Err(compatibility_error(format!(
                "table '{table}' is missing required trigger '{name}'"
            )));
        }
    }
    Ok(())
}

async fn verify_index_extensions(
    connection: &Connection,
    table: &str,
    expected: &SchemaSnapshot,
) -> Result<(), PersistenceError> {
    let mut rows = connection
        .query(
            "SELECT name FROM sqlite_schema
             WHERE type = 'index' AND sql IS NOT NULL
               AND name NOT GLOB 'sqlite_*' AND tbl_name = ?1
             ORDER BY name",
            [table],
        )
        .await
        .map_err(|error| schema_query_error("list table indexes", error))?;
    let mut names = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| schema_query_error("read table index", error))?
    {
        names.push(
            row.get::<String>(0)
                .map_err(|error| schema_query_error("decode table index name", error))?,
        );
    }
    drop(rows);

    for name in names {
        if expected.indexes.contains_key(&name) {
            continue;
        }
        let actual = index_capability(connection, &name).await?;
        if !is_safe_plain_index(table, &actual) {
            return Err(compatibility_error(format!(
                "table '{table}' has unexpected executable index extension '{name}' with semantics {actual:?}"
            )));
        }
    }
    Ok(())
}

fn is_safe_plain_index(table: &str, index: &IndexCapability) -> bool {
    index.table == table
        && !index.unique
        && !index.partial
        && index.predicate.is_none()
        && !index.columns.is_empty()
        && index.columns.iter().all(|column| {
            column.name.is_some()
                && matches!(
                    column.collation.as_deref(),
                    None | Some("binary" | "nocase" | "rtrim")
                )
        })
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

fn schema_query_error(context: &str, error: libsql::Error) -> PersistenceError {
    PersistenceError::Storage(format!(
        "Turso schema introspection failed while attempting to {context}: {error} ({error:?})"
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use libsql::Builder;

    use super::{SchemaSnapshot, verify_index_extensions};

    #[tokio::test]
    async fn case_folded_index_owner_is_inventoried() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database = Builder::new_local(directory.path().join("case-folded-index-owner.db"))
            .build()
            .await
            .expect("build temporary database");
        let connection = database.connect().expect("connect temporary database");
        connection
            .execute("CREATE TABLE EVENTS(payload TEXT NOT NULL)", ())
            .await
            .expect("create differently cased table owner");
        connection
            .execute(
                "CREATE INDEX events_case_folded_expression
                 ON EVENTS(json_extract(payload, 'invalid-path'))",
                (),
            )
            .await
            .expect("create expression index with differently cased owner");
        let expected = SchemaSnapshot {
            tables: BTreeMap::new(),
            indexes: BTreeMap::new(),
            triggers: BTreeMap::new(),
        };

        let error = verify_index_extensions(&connection, "events", &expected)
            .await
            .expect_err("SQLite-equivalent index owners must be inventoried");
        assert!(
            error
                .to_string()
                .contains("events_case_folded_expression"),
            "{error}"
        );
    }
}
