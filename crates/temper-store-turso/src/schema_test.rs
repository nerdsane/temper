use super::*;

#[test]
fn schemas_are_idempotent() {
    assert!(CREATE_EVENTS_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_EVENTS_ENTITY_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_EVENT_SEGMENTS_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_EVENT_SEGMENTS_OPEN_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_SNAPSHOTS_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_SNAPSHOT_HISTORY_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_SNAPSHOT_HISTORY_ENTITY_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_SPECS_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_TRAJECTORIES_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_TRAJECTORIES_SUCCESS_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_TRAJECTORIES_ENTITY_ACTION_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_TENANT_CONSTRAINTS_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_WASM_MODULES_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_WASM_INVOCATION_LOGS_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_WASM_INVOCATION_LOGS_TENANT_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_WASM_INVOCATION_LOGS_MODULE_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_WASM_INVOCATION_LOGS_CREATED_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_PENDING_DECISIONS_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_POLICY_PUBLICATIONS_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_PENDING_DECISIONS_TENANT_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_PENDING_DECISIONS_STATUS_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_TRAJECTORIES_AGENT_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_FEATURE_REQUESTS_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_EVOLUTION_RECORDS_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_EVOLUTION_RECORDS_TYPE_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_EVOLUTION_RECORDS_STATUS_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_DESIGN_TIME_EVENTS_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_DESIGN_TIME_EVENTS_TENANT_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_TENANT_SECRETS_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_OTS_TRAJECTORIES_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_OTS_TRAJECTORIES_AGENT_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_OTS_TRAJECTORIES_TENANT_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_OTS_TRAJECTORIES_OUTCOME_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_ENTITY_CATALOG_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_ENTITY_CATALOG_TYPE_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_ENTITY_CATALOG_STATUS_INDEX.contains("IF NOT EXISTS"));
    assert!(CREATE_ENTITY_FIELD_INDEX_TABLE.contains("IF NOT EXISTS"));
    assert!(CREATE_ENTITY_FIELD_INDEX_LOOKUP.contains("IF NOT EXISTS"));
    assert!(CREATE_ENTITY_FIELD_INDEX_STATUS.contains("IF NOT EXISTS"));
}

#[test]
fn event_history_schema_declares_segments_and_snapshot_history() {
    assert!(
        CREATE_EVENTS_TABLE.contains("segment_index"),
        "events rows must carry segment metadata"
    );
    assert!(
        ALTER_EVENTS_ADD_SEGMENT_INDEX.contains("ADD COLUMN segment_index"),
        "existing unsegmented event rows need an idempotent segment migration"
    );
    assert!(
        CREATE_EVENT_SEGMENTS_TABLE.contains("CREATE TABLE IF NOT EXISTS event_segments"),
        "event segment metadata table is required"
    );
    assert!(
        CREATE_SNAPSHOT_HISTORY_TABLE.contains("CREATE TABLE IF NOT EXISTS snapshot_history"),
        "immutable snapshot history table is required"
    );
}

#[test]
fn wasm_modules_table_has_required_columns() {
    let sql = CREATE_WASM_MODULES_TABLE.to_uppercase();
    for col in &[
        "TENANT",
        "MODULE_NAME",
        "WASM_BYTES",
        "SHA256_HASH",
        "VERSION",
        "SIZE_BYTES",
    ] {
        assert!(
            sql.contains(col),
            "wasm_modules schema missing column: {col}"
        );
    }
}

#[test]
fn entity_catalog_stores_full_fields_json() {
    let sql = CREATE_ENTITY_CATALOG_TABLE.to_uppercase();
    assert!(
        sql.contains("FIELDS"),
        "entity_catalog schema must preserve full projected fields for catalog reads"
    );
}
