use libsql::{Connection, params};

use super::{create_events, ledger_count, scalar_i64, schema_kind, temporary_connection};
use crate::migrations::catalog::{MIGRATIONS, Migration, MigrationStep};
use crate::migrations::runner::{migrate, migrate_catalog};
use crate::store::ots::PERSIST_OTS_TRAJECTORY_SQL;

const TIGHTEN_EVENTS_STEPS: &[MigrationStep] = &[MigrationStep::Sql(
    "ALTER TABLE events ADD COLUMN required_value TEXT NOT NULL DEFAULT 'x'",
)];
const DECLARED_TRIGGER_STEPS: &[MigrationStep] = &[MigrationStep::Sql(
    "CREATE TRIGGER catalog_events_audit AFTER INSERT ON events BEGIN SELECT 1; END",
)];

#[tokio::test]
async fn later_migration_can_tighten_an_earlier_owned_table() {
    let (_directory, connection) = temporary_connection("later-table-tightening").await;
    migrate(&connection)
        .await
        .expect("install released catalog");
    assert_eq!(ledger_count(&connection).await, MIGRATIONS.len() as i64);

    let mut catalog = MIGRATIONS.to_vec();
    catalog.push(Migration {
        version: 8,
        name: "tighten-event-journal",
        steps: TIGHTEN_EVENTS_STEPS,
    });

    migrate_catalog(&connection, &catalog)
        .await
        .expect("the latest catalog schema must define readiness");
    migrate_catalog(&connection, &catalog)
        .await
        .expect("the tightened head schema must remain replay-safe");

    assert_eq!(ledger_count(&connection).await, 8);
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM pragma_table_xinfo('events')
             WHERE name = 'required_value' AND \"notnull\" = 1 AND dflt_value = '''x'''",
        )
        .await,
        1
    );
}

#[tokio::test]
async fn unexpected_trigger_prevents_ledgering_and_is_preserved() {
    let (_directory, connection) = temporary_connection("unexpected-trigger").await;
    create_events(&connection, None, None).await;
    connection
        .execute(
            "CREATE TRIGGER reject_events BEFORE INSERT ON events
             BEGIN SELECT RAISE(FAIL, 'blocked'); END",
            (),
        )
        .await
        .expect("create blocking trigger");
    let before = trigger_sql(&connection, "reject_events").await;

    let error = migrate(&connection)
        .await
        .expect_err("an unexpected runtime-table trigger must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(diagnostic.contains("trigger"), "{diagnostic}");
    assert_eq!(trigger_sql(&connection, "reject_events").await, before);
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn differently_cased_trigger_owner_cannot_bypass_inventory() {
    let (_directory, connection) = temporary_connection("case-folded-trigger-owner").await;
    create_events(&connection, None, None).await;
    connection
        .execute(
            "CREATE TRIGGER reject_case_folded_events BEFORE INSERT ON EVENTS
             BEGIN SELECT RAISE(FAIL, 'blocked'); END",
            (),
        )
        .await
        .expect("create blocking trigger with differently cased owner");
    let before = trigger_sql(&connection, "reject_case_folded_events").await;

    let error = migrate(&connection)
        .await
        .expect_err("SQLite-equivalent trigger owners must not bypass readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(
        diagnostic.contains("reject_case_folded_events"),
        "{diagnostic}"
    );
    assert_eq!(
        trigger_sql(&connection, "reject_case_folded_events").await,
        before
    );
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn sqlite_x_named_trigger_cannot_bypass_inventory() {
    let (_directory, connection) = temporary_connection("sqlite-x-trigger").await;
    create_events(&connection, None, None).await;
    connection
        .execute(
            "CREATE TRIGGER sqliteXreject_events BEFORE INSERT ON events
             BEGIN SELECT RAISE(FAIL, 'blocked'); END",
            (),
        )
        .await
        .expect("create legally named blocking trigger");

    let error = migrate(&connection)
        .await
        .expect_err("a sqliteX-prefixed trigger must still prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(diagnostic.contains("sqliteXreject_events"), "{diagnostic}");
    assert_eq!(
        schema_kind(&connection, "sqliteXreject_events")
            .await
            .as_deref(),
        Some("trigger")
    );
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn probe_identity_bypass_trigger_prevents_readiness() {
    let (_directory, connection) = temporary_connection("probe-identity-bypass-trigger").await;
    migrate(&connection).await.expect("install current catalog");
    connection
        .execute(
            "CREATE TRIGGER reject_non_probe_tenant BEFORE INSERT ON ots_trajectories
             WHEN NEW.tenant <> '__temper_trigger_probe__'
             BEGIN SELECT RAISE(FAIL, 'real tenant blocked'); END",
            (),
        )
        .await
        .expect("create trigger that recognizes the probe tenant");

    let runtime_error = connection
        .execute(
            PERSIST_OTS_TRAJECTORY_SQL,
            params![
                "real-trajectory",
                "tenant-a",
                "agent-a",
                "session-a",
                "outcome-a",
                1_i64,
                "{}",
            ],
        )
        .await
        .expect_err("the trigger must reproduce the real-write failure");
    assert!(
        runtime_error.to_string().contains("real tenant blocked"),
        "{runtime_error}"
    );

    let error = migrate(&connection)
        .await
        .expect_err("a trigger that distinguishes probe inputs must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 7"), "{diagnostic}");
    assert!(
        diagnostic.contains("reject_non_probe_tenant"),
        "{diagnostic}"
    );
    assert_eq!(ledger_count(&connection).await, MIGRATIONS.len() as i64);
}

#[tokio::test]
async fn unmodeled_ots_trigger_mutation_prevents_readiness() {
    let (_directory, connection) = temporary_connection("unmodeled-ots-trigger-mutation").await;
    migrate(&connection).await.expect("install current catalog");
    connection
        .execute(
            "CREATE TRIGGER mutate_ots_entity_type AFTER INSERT ON ots_trajectories
             BEGIN
                 UPDATE ots_trajectories
                 SET entity_type = 'trigger-corruption'
                 WHERE trajectory_id = NEW.trajectory_id;
             END",
            (),
        )
        .await
        .expect("create trigger that mutates an unasserted OTS column");

    connection
        .execute(
            PERSIST_OTS_TRAJECTORY_SQL,
            params![
                "mutated-trajectory",
                "tenant-a",
                "agent-a",
                "session-a",
                "outcome-a",
                1_i64,
                "{}",
            ],
        )
        .await
        .expect("the trigger leaves the currently asserted production fields writable");
    assert_eq!(
        scalar_text(
            &connection,
            "SELECT entity_type FROM ots_trajectories
             WHERE trajectory_id = 'mutated-trajectory'",
        )
        .await
        .as_deref(),
        Some("trigger-corruption"),
        "the trigger must reproduce an unasserted mutation"
    );

    let error = migrate(&connection)
        .await
        .expect_err("a trigger outside the supported audit contract must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 7"), "{diagnostic}");
    assert!(
        diagnostic.contains("mutate_ots_entity_type"),
        "{diagnostic}"
    );
    assert_eq!(ledger_count(&connection).await, MIGRATIONS.len() as i64);
}

#[tokio::test]
async fn blocking_legacy_ots_trigger_fails_the_runtime_write_probe() {
    let (_directory, connection) = temporary_connection("blocking-ots-trigger").await;
    migrate(&connection).await.expect("install current catalog");
    connection
        .execute(
            "CREATE TRIGGER reject_ots_insert AFTER INSERT ON ots_trajectories
             BEGIN SELECT RAISE(FAIL, 'blocked'); END",
            (),
        )
        .await
        .expect("create blocking OTS trigger");
    let before = trigger_sql(&connection, "reject_ots_insert").await;

    let error = migrate(&connection)
        .await
        .expect_err("a trigger that rejects canonical OTS writes must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 7"), "{diagnostic}");
    assert!(diagnostic.contains("reject_ots_insert"), "{diagnostic}");
    assert!(
        diagnostic.contains("production persist/enqueue/status-transition probe"),
        "{diagnostic}"
    );
    assert_eq!(trigger_sql(&connection, "reject_ots_insert").await, before);
    assert_eq!(ledger_count(&connection).await, MIGRATIONS.len() as i64);
}

#[tokio::test]
async fn queued_only_ots_trigger_fails_the_production_write_probe() {
    let (_directory, connection) = temporary_connection("queued-ots-trigger").await;
    migrate(&connection).await.expect("install current catalog");
    connection
        .execute(
            "CREATE TRIGGER reject_queued_ots BEFORE INSERT ON ots_trajectories
             WHEN NEW.persistence_status = 'queued'
             BEGIN SELECT RAISE(FAIL, 'queued blocked'); END",
            (),
        )
        .await
        .expect("create queued-only OTS trigger");

    let error = migrate(&connection)
        .await
        .expect_err("the probe must exercise the production queued insert path");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 7"), "{diagnostic}");
    assert!(diagnostic.contains("reject_queued_ots"), "{diagnostic}");
    assert!(
        diagnostic.contains("probe OTS enqueue insert"),
        "{diagnostic}"
    );
    assert_eq!(ledger_count(&connection).await, MIGRATIONS.len() as i64);
}

#[tokio::test]
async fn benign_legacy_ots_trigger_probe_has_no_durable_side_effects() {
    let (_directory, connection) = temporary_connection("benign-ots-trigger").await;
    migrate(&connection).await.expect("install current catalog");
    connection
        .execute(
            "CREATE TABLE ots_probe_audit (trajectory_id TEXT PRIMARY KEY)",
            (),
        )
        .await
        .expect("create OTS audit table");
    connection
        .execute(
            "CREATE TRIGGER audit_ots_insert AFTER INSERT ON ots_trajectories
             BEGIN
                INSERT INTO ots_probe_audit (trajectory_id) VALUES (NEW.trajectory_id);
             END",
            (),
        )
        .await
        .expect("create benign OTS trigger");

    migrate(&connection)
        .await
        .expect("a canonical-write-compatible OTS trigger must remain supported");
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM ots_probe_audit").await,
        0,
        "the validation probe and its trigger side effects must roll back"
    );

    connection
        .execute(
            "INSERT INTO ots_trajectories (trajectory_id, tenant, agent_id, data)
             VALUES ('runtime-trajectory', 'tenant-a', 'agent-a', '{}')",
            (),
        )
        .await
        .expect("runtime write through validated OTS trigger");
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM ots_probe_audit
             WHERE trajectory_id = 'runtime-trajectory'",
        )
        .await,
        1
    );
    assert_eq!(ledger_count(&connection).await, MIGRATIONS.len() as i64);
}

#[tokio::test]
async fn unexpected_expression_index_prevents_ledgering_and_is_preserved() {
    let (_directory, connection) = temporary_connection("unexpected-expression-index").await;
    create_events(&connection, None, None).await;
    connection
        .execute(
            "CREATE INDEX events_unexpected_expression
             ON events(json_extract(payload, 'invalid-path'))",
            (),
        )
        .await
        .expect("create executable expression index");

    let error = migrate(&connection)
        .await
        .expect_err("an executable expression index must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(
        diagnostic.contains("events_unexpected_expression"),
        "{diagnostic}"
    );
    assert_eq!(
        schema_kind(&connection, "events_unexpected_expression")
            .await
            .as_deref(),
        Some("index")
    );
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn sqlite_x_named_expression_index_cannot_bypass_inventory() {
    let (_directory, connection) = temporary_connection("sqlite-x-expression-index").await;
    create_events(&connection, None, None).await;
    connection
        .execute(
            "CREATE INDEX sqliteXevents_expression
             ON events(json_extract(payload, 'invalid-path'))",
            (),
        )
        .await
        .expect("create legally named expression index");

    let error = migrate(&connection)
        .await
        .expect_err("a sqliteX-prefixed expression index must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(
        diagnostic.contains("sqliteXevents_expression"),
        "{diagnostic}"
    );
    assert_eq!(
        schema_kind(&connection, "sqliteXevents_expression")
            .await
            .as_deref(),
        Some("index")
    );
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn unexpected_partial_index_prevents_ledgering_and_is_preserved() {
    let (_directory, connection) = temporary_connection("unexpected-partial-index").await;
    create_events(&connection, None, None).await;
    connection
        .execute(
            "CREATE INDEX events_unexpected_partial ON events(event_type)
             WHERE json_extract(payload, 'invalid-path')",
            (),
        )
        .await
        .expect("create executable partial index");

    let error = migrate(&connection)
        .await
        .expect_err("an executable partial index must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(
        diagnostic.contains("events_unexpected_partial"),
        "{diagnostic}"
    );
    assert_eq!(
        schema_kind(&connection, "events_unexpected_partial")
            .await
            .as_deref(),
        Some("index")
    );
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn plain_non_unique_index_extension_remains_compatible() {
    let (_directory, connection) = temporary_connection("plain-index-extension").await;
    create_events(&connection, None, None).await;
    connection
        .execute(
            "CREATE INDEX events_deployment_lookup ON events(event_type DESC, tenant)",
            (),
        )
        .await
        .expect("create plain index extension");

    migrate(&connection)
        .await
        .expect("a plain non-unique index extension must remain compatible");
    connection
        .execute(
            "INSERT INTO events (
                tenant, entity_type, entity_id, sequence_nr, event_type, payload
             ) VALUES ('tenant-a', 'Order', 'order-1', 1, 'Created', '{}')",
            (),
        )
        .await
        .expect("canonical runtime insert with plain index extension");
    assert_eq!(ledger_count(&connection).await, MIGRATIONS.len() as i64);
}

#[tokio::test]
async fn declared_trigger_mismatch_and_absence_prevent_replay() {
    let (_directory, connection) = temporary_connection("declared-trigger").await;
    migrate(&connection)
        .await
        .expect("install released catalog");
    let mut catalog = MIGRATIONS.to_vec();
    catalog.push(Migration {
        version: 8,
        name: "declare-events-trigger",
        steps: DECLARED_TRIGGER_STEPS,
    });
    migrate_catalog(&connection, &catalog)
        .await
        .expect("install declared trigger migration");

    connection
        .execute("DROP TRIGGER catalog_events_audit", ())
        .await
        .expect("drop declared trigger");
    connection
        .execute(
            "CREATE TRIGGER catalog_events_audit AFTER DELETE ON events BEGIN SELECT 1; END",
            (),
        )
        .await
        .expect("install mismatched trigger definition");
    let mismatch = migrate_catalog(&connection, &catalog)
        .await
        .expect_err("a changed declared trigger must prevent readiness");
    let mismatch_diagnostic = mismatch.to_string();
    assert!(
        mismatch_diagnostic.contains("migration 8"),
        "{mismatch_diagnostic}"
    );
    assert!(
        mismatch_diagnostic.contains("incompatible semantics"),
        "{mismatch_diagnostic}"
    );

    connection
        .execute("DROP TRIGGER catalog_events_audit", ())
        .await
        .expect("remove mismatched trigger");
    let missing = migrate_catalog(&connection, &catalog)
        .await
        .expect_err("a missing declared trigger must prevent readiness");
    let missing_diagnostic = missing.to_string();
    assert!(
        missing_diagnostic.contains("migration 8"),
        "{missing_diagnostic}"
    );
    assert!(
        missing_diagnostic.contains("missing required trigger"),
        "{missing_diagnostic}"
    );
    assert_eq!(ledger_count(&connection).await, 8);
}

async fn trigger_sql(connection: &Connection, trigger: &str) -> String {
    let mut rows = connection
        .query(
            "SELECT sql FROM sqlite_schema WHERE type = 'trigger' AND name = ?1",
            [trigger],
        )
        .await
        .expect("query trigger SQL");
    rows.next()
        .await
        .expect("read trigger SQL")
        .expect("trigger SQL row")
        .get::<String>(0)
        .expect("decode trigger SQL")
}

async fn scalar_text(connection: &Connection, sql: &str) -> Option<String> {
    let mut rows = connection.query(sql, ()).await.expect("query text scalar");
    rows.next()
        .await
        .expect("read text scalar")
        .expect("text scalar row")
        .get::<Option<String>>(0)
        .expect("decode text scalar")
}
