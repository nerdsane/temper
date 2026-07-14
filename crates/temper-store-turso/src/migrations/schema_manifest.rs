use super::schema_snapshot::{IndexColumn, SchemaSnapshot};

pub(super) fn canonical_manifest(snapshot: &SchemaSnapshot) -> String {
    let mut manifest = String::new();
    part(&mut manifest, "temper-schema-capability-manifest-v1");
    count(&mut manifest, snapshot.tables.len());
    for (table_name, table) in &snapshot.tables {
        part(&mut manifest, table_name);
        count(&mut manifest, table.columns.len());
        for (column_name, column) in &table.columns {
            part(&mut manifest, column_name);
            part(&mut manifest, &column.affinity);
            boolean(&mut manifest, column.not_null);
            optional(&mut manifest, column.default.as_deref());
            integer(&mut manifest, column.primary_key_position);
        }

        count(&mut manifest, table.unique_keys.len());
        for key in &table.unique_keys {
            index_columns(&mut manifest, key);
        }

        count(&mut manifest, table.foreign_keys.len());
        for foreign_key in &table.foreign_keys {
            integer(&mut manifest, foreign_key.id);
            integer(&mut manifest, foreign_key.sequence);
            part(&mut manifest, &foreign_key.target_table);
            part(&mut manifest, &foreign_key.source_column);
            optional(&mut manifest, foreign_key.target_column.as_deref());
            part(&mut manifest, &foreign_key.on_update);
            part(&mut manifest, &foreign_key.on_delete);
            part(&mut manifest, &foreign_key.match_kind);
        }
    }

    count(&mut manifest, snapshot.indexes.len());
    for (index_name, index) in &snapshot.indexes {
        part(&mut manifest, index_name);
        part(&mut manifest, &index.table);
        boolean(&mut manifest, index.unique);
        boolean(&mut manifest, index.partial);
        index_columns(&mut manifest, &index.columns);
        optional(&mut manifest, index.predicate.as_deref());
    }
    manifest
}

fn index_columns(manifest: &mut String, columns: &[IndexColumn]) {
    count(manifest, columns.len());
    for column in columns {
        optional(manifest, column.name.as_deref());
        boolean(manifest, column.descending);
        optional(manifest, column.collation.as_deref());
    }
}

fn optional(manifest: &mut String, value: Option<&str>) {
    match value {
        Some(value) => {
            part(manifest, "some");
            part(manifest, value);
        }
        None => part(manifest, "none"),
    }
}

fn boolean(manifest: &mut String, value: bool) {
    part(manifest, if value { "true" } else { "false" });
}

fn integer(manifest: &mut String, value: i64) {
    part(manifest, &value.to_string());
}

fn count(manifest: &mut String, value: usize) {
    part(manifest, &value.to_string());
}

fn part(manifest: &mut String, value: &str) {
    manifest.push_str(&value.len().to_string());
    manifest.push(':');
    manifest.push_str(value);
}
