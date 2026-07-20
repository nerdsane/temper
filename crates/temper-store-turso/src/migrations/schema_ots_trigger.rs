use libsql::Connection;
use temper_runtime::persistence::PersistenceError;

use super::schema_snapshot::{
    IndexColumn, UniqueKeyCapability, compatibility_error, index_capability, table_capability,
};
use super::schema_sql::{canonical_tokens, normalize_schema_ddl};
use super::schema_trigger::{TriggerCapability, capture_triggers};

const AUDIT_COLUMN: &str = "trajectory_id";

struct AuditSink {
    table: String,
}

pub(super) async fn validate_ots_audit_trigger_contracts(
    connection: &Connection,
    triggers: &[(&str, &TriggerCapability)],
) -> Result<(), PersistenceError> {
    for (name, trigger) in triggers {
        let sink = parse_audit_sink(name, trigger).ok_or_else(|| {
            compatibility_error(format!(
                "table 'ots_trajectories' has unsupported executable trigger extension \
                 '{name}'; legacy extensions must be an unconditional AFTER INSERT audit \
                 trigger with exactly one INSERT INTO <audit_table>(trajectory_id) \
                 VALUES (NEW.trajectory_id) statement"
            ))
        })?;
        validate_audit_sink(connection, name, &sink).await?;
    }
    Ok(())
}

fn parse_audit_sink(name: &str, trigger: &TriggerCapability) -> Option<AuditSink> {
    if trigger.table != "ots_trajectories" {
        return None;
    }
    let tokens = canonical_tokens(&trigger.definition);
    let expected_name = name.to_ascii_lowercase();
    if tokens.len() != 22
        || tokens[0] != "create"
        || tokens[1] != "trigger"
        || tokens[2] != expected_name
        || tokens[3] != "after"
        || tokens[4] != "insert"
        || tokens[5] != "on"
        || tokens[6] != "ots_trajectories"
        || tokens[7] != "begin"
        || tokens[8] != "insert"
        || tokens[9] != "into"
        || !is_plain_identifier(&tokens[10])
        || tokens[11] != "("
        || tokens[12] != AUDIT_COLUMN
        || tokens[13] != ")"
        || tokens[14] != "values"
        || tokens[15] != "("
        || tokens[16] != "new"
        || tokens[17] != "."
        || tokens[18] != AUDIT_COLUMN
        || tokens[19] != ")"
        || tokens[20] != ";"
        || tokens[21] != "end"
    {
        return None;
    }
    Some(AuditSink {
        table: tokens[10].clone(),
    })
}

async fn validate_audit_sink(
    connection: &Connection,
    trigger_name: &str,
    sink: &AuditSink,
) -> Result<(), PersistenceError> {
    let (actual_name, definition) = audit_table_definition(connection, &sink.table).await?;
    if !normalize_schema_ddl(&definition).starts_with("create table ") {
        return Err(unsupported_sink(
            trigger_name,
            &actual_name,
            "the sink is not a plain table",
        ));
    }

    let table = table_capability(connection, &actual_name).await?;
    let column = table
        .columns
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(AUDIT_COLUMN));
    if table.columns.len() != 1
        || column.is_none_or(|(_, column)| {
            column.affinity != "TEXT" || column.default.is_some() || column.hidden != 0
        })
    {
        return Err(unsupported_sink(
            trigger_name,
            &actual_name,
            "the sink must contain only one visible TEXT trajectory_id column without a default",
        ));
    }
    if !table.foreign_keys.is_empty() || !table.restricted_semantics.is_empty() {
        return Err(unsupported_sink(
            trigger_name,
            &actual_name,
            "the sink must not execute foreign keys, checks, generated columns, collations, or other table restrictions",
        ));
    }
    if !table.unique_keys.iter().all(is_safe_trajectory_key) {
        return Err(unsupported_sink(
            trigger_name,
            &actual_name,
            "the sink has an unsafe unique-key definition",
        ));
    }
    if !capture_triggers(connection, Some(&actual_name))
        .await?
        .is_empty()
    {
        return Err(unsupported_sink(
            trigger_name,
            &actual_name,
            "the sink has executable triggers",
        ));
    }
    validate_audit_indexes(connection, trigger_name, &actual_name).await
}

async fn validate_audit_indexes(
    connection: &Connection,
    trigger_name: &str,
    table: &str,
) -> Result<(), PersistenceError> {
    let mut rows = connection
        .query(
            "SELECT name FROM sqlite_schema
             WHERE type = 'index' AND sql IS NOT NULL
               AND name NOT GLOB 'sqlite_*' AND tbl_name COLLATE NOCASE = ?1
             ORDER BY name",
            [table],
        )
        .await
        .map_err(|error| sink_query_error("list audit sink indexes", error))?;
    let mut names = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| sink_query_error("read audit sink index", error))?
    {
        names.push(
            row.get::<String>(0)
                .map_err(|error| sink_query_error("decode audit sink index name", error))?,
        );
    }
    drop(rows);

    for name in names {
        let index = index_capability(connection, &name).await?;
        if index.partial
            || index.predicate.is_some()
            || index.columns.len() != 1
            || !is_safe_trajectory_column(&index.columns[0])
        {
            return Err(unsupported_sink(
                trigger_name,
                table,
                &format!("the sink has executable or non-canonical index '{name}'"),
            ));
        }
    }
    Ok(())
}

async fn audit_table_definition(
    connection: &Connection,
    table: &str,
) -> Result<(String, String), PersistenceError> {
    let mut rows = connection
        .query(
            "SELECT name, sql FROM sqlite_schema
             WHERE type = 'table' AND name COLLATE NOCASE = ?1
             ORDER BY name LIMIT 1",
            [table],
        )
        .await
        .map_err(|error| sink_query_error("inspect audit sink table", error))?;
    let row = rows
        .next()
        .await
        .map_err(|error| sink_query_error("read audit sink table", error))?
        .ok_or_else(|| {
            compatibility_error(format!(
                "OTS audit trigger references missing table '{table}'"
            ))
        })?;
    Ok((
        row.get::<String>(0)
            .map_err(|error| sink_query_error("decode audit sink table name", error))?,
        row.get::<String>(1)
            .map_err(|error| sink_query_error("decode audit sink table definition", error))?,
    ))
}

fn is_safe_trajectory_key(key: &UniqueKeyCapability) -> bool {
    !key.partial
        && key.predicate.is_none()
        && key.columns.len() == 1
        && is_safe_trajectory_column(&key.columns[0])
}

fn is_safe_trajectory_column(column: &IndexColumn) -> bool {
    column
        .name
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case(AUDIT_COLUMN))
        && !column.descending
        && matches!(column.collation.as_deref(), None | Some("binary"))
}

fn is_plain_identifier(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with("sqlite_")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
}

fn unsupported_sink(trigger: &str, table: &str, reason: &str) -> PersistenceError {
    compatibility_error(format!(
        "table 'ots_trajectories' trigger extension '{trigger}' has unsupported audit sink \
         '{table}': {reason}"
    ))
}

fn sink_query_error(context: &str, error: libsql::Error) -> PersistenceError {
    PersistenceError::Storage(format!(
        "Turso schema introspection failed while attempting to {context}: {error} ({error:?})"
    ))
}

#[cfg(test)]
mod tests {
    use super::parse_audit_sink;
    use crate::migrations::schema_sql::normalize_schema_ddl;
    use crate::migrations::schema_trigger::TriggerCapability;

    fn trigger(definition: &str) -> TriggerCapability {
        TriggerCapability {
            table: "ots_trajectories".into(),
            definition: normalize_schema_ddl(definition),
        }
    }

    #[test]
    fn parses_exact_unconditional_audit_contract() {
        let capability = trigger(
            "CREATE TRIGGER audit_ots_insert AFTER INSERT ON ots_trajectories
             BEGIN
                 INSERT INTO ots_audit (trajectory_id) VALUES (NEW.trajectory_id);
             END",
        );
        let sink = parse_audit_sink("audit_ots_insert", &capability).expect("audit contract");
        assert_eq!(sink.table, "ots_audit");
    }

    #[test]
    fn rejects_conditions_and_additional_statements() {
        let conditional = trigger(
            "CREATE TRIGGER audit_ots_insert AFTER INSERT ON ots_trajectories
             WHEN NEW.tenant = 'probe'
             BEGIN INSERT INTO ots_audit (trajectory_id) VALUES (NEW.trajectory_id); END",
        );
        assert!(parse_audit_sink("audit_ots_insert", &conditional).is_none());

        let mutating = trigger(
            "CREATE TRIGGER audit_ots_insert AFTER INSERT ON ots_trajectories
             BEGIN
                 INSERT INTO ots_audit (trajectory_id) VALUES (NEW.trajectory_id);
                 UPDATE ots_trajectories SET entity_type = 'changed';
             END",
        );
        assert!(parse_audit_sink("audit_ots_insert", &mutating).is_none());
    }
}
