//! SQL schema definitions for the PostgreSQL event store.
//!
//! These constants define the DDL statements used to create the `events` and
//! `snapshots` tables that back the event-sourced persistence layer.

/// CREATE TABLE statement for the events journal.
///
/// Each row stores a single domain event for a particular entity. The
/// `(entity_type, entity_id, sequence_nr)` UNIQUE constraint enforces
/// optimistic concurrency — two writers attempting to persist the same
/// sequence number will conflict, and only one will succeed.
pub const CREATE_EVENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS events (
    id            BIGSERIAL    NOT NULL,
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL,
    sequence_nr   BIGINT       NOT NULL,
    event_type    TEXT         NOT NULL,
    payload       JSONB        NOT NULL,
    metadata      JSONB        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (id),
    UNIQUE (entity_type, entity_id, sequence_nr)
);";

/// CREATE TABLE statement for the snapshots table.
///
/// Snapshots store the serialised state of an entity at a given sequence
/// number. Only the latest snapshot per entity is kept — the UPSERT in
/// `save_snapshot` replaces older rows via the composite primary key.
pub const CREATE_SNAPSHOTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS snapshots (
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL,
    sequence_nr   BIGINT       NOT NULL,
    state         BYTEA        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (entity_type, entity_id)
);";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_table_contains_required_columns() {
        let sql = CREATE_EVENTS_TABLE.to_uppercase();
        for col in &[
            "ENTITY_TYPE",
            "ENTITY_ID",
            "SEQUENCE_NR",
            "EVENT_TYPE",
            "PAYLOAD",
            "METADATA",
            "CREATED_AT",
        ] {
            assert!(
                sql.contains(col),
                "events schema missing column: {col}"
            );
        }
    }

    #[test]
    fn events_table_has_unique_constraint() {
        let sql = CREATE_EVENTS_TABLE.to_uppercase();
        assert!(
            sql.contains("UNIQUE"),
            "events schema should enforce a UNIQUE constraint on (entity_type, entity_id, sequence_nr)"
        );
        // The three columns must appear together inside the UNIQUE clause.
        let unique_pos = sql.find("UNIQUE").unwrap();
        let after_unique = &sql[unique_pos..];
        assert!(after_unique.contains("ENTITY_TYPE"), "UNIQUE constraint missing entity_type");
        assert!(after_unique.contains("ENTITY_ID"), "UNIQUE constraint missing entity_id");
        assert!(after_unique.contains("SEQUENCE_NR"), "UNIQUE constraint missing sequence_nr");
    }

    #[test]
    fn snapshots_table_contains_required_columns() {
        let sql = CREATE_SNAPSHOTS_TABLE.to_uppercase();
        for col in &["ENTITY_TYPE", "ENTITY_ID", "SEQUENCE_NR", "STATE", "CREATED_AT"] {
            assert!(
                sql.contains(col),
                "snapshots schema missing column: {col}"
            );
        }
    }

    #[test]
    fn snapshots_table_has_composite_primary_key() {
        let sql = CREATE_SNAPSHOTS_TABLE.to_uppercase();
        let pk_pos = sql.find("PRIMARY KEY").expect("snapshots schema missing PRIMARY KEY");
        let after_pk = &sql[pk_pos..];
        assert!(after_pk.contains("ENTITY_TYPE"), "PRIMARY KEY missing entity_type");
        assert!(after_pk.contains("ENTITY_ID"), "PRIMARY KEY missing entity_id");
    }

    #[test]
    fn events_table_uses_jsonb_for_payload_and_metadata() {
        let sql = CREATE_EVENTS_TABLE.to_uppercase();
        // Find PAYLOAD line and verify it uses JSONB
        assert!(sql.contains("PAYLOAD") && sql.contains("JSONB"), "payload should be JSONB");
    }

    #[test]
    fn snapshots_table_uses_bytea_for_state() {
        let sql = CREATE_SNAPSHOTS_TABLE.to_uppercase();
        assert!(sql.contains("STATE") && sql.contains("BYTEA"), "state should be BYTEA");
    }

    #[test]
    fn schemas_use_if_not_exists() {
        assert!(
            CREATE_EVENTS_TABLE.contains("IF NOT EXISTS"),
            "events schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_SNAPSHOTS_TABLE.contains("IF NOT EXISTS"),
            "snapshots schema should use IF NOT EXISTS"
        );
    }
}
