use std::collections::BTreeMap;

use libsql::Connection;
use temper_runtime::persistence::PersistenceError;

use super::schema_sql::normalize_schema_ddl;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TriggerCapability {
    pub table: String,
    pub definition: String,
}

pub(super) async fn capture_triggers(
    connection: &Connection,
    table: Option<&str>,
) -> Result<BTreeMap<String, TriggerCapability>, PersistenceError> {
    let mut rows = if let Some(table) = table {
        connection
            .query(
                "SELECT name, tbl_name, sql FROM sqlite_schema
                 WHERE type = 'trigger' AND name NOT GLOB 'sqlite_*' AND tbl_name = ?1
                 ORDER BY name",
                [table],
            )
            .await
    } else {
        connection
            .query(
                "SELECT name, tbl_name, sql FROM sqlite_schema
                 WHERE type = 'trigger' AND name NOT GLOB 'sqlite_*'
                 ORDER BY name",
                (),
            )
            .await
    }
    .map_err(|error| trigger_query_error("list triggers", error))?;

    let mut triggers = BTreeMap::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| trigger_query_error("read trigger", error))?
    {
        let name = row
            .get::<String>(0)
            .map_err(|error| trigger_query_error("decode trigger name", error))?;
        let owner = row
            .get::<String>(1)
            .map_err(|error| trigger_query_error("decode trigger owner", error))?;
        let definition = row
            .get::<String>(2)
            .map_err(|error| trigger_query_error("decode trigger definition", error))?;
        triggers.insert(
            name,
            TriggerCapability {
                table: owner,
                definition: normalize_schema_ddl(&definition),
            },
        );
    }
    Ok(triggers)
}

fn trigger_query_error(context: &str, error: libsql::Error) -> PersistenceError {
    PersistenceError::Storage(format!(
        "Turso schema introspection failed while attempting to {context}: {error} ({error:?})"
    ))
}
