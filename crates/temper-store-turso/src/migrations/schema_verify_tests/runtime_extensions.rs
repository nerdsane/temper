use libsql::Connection;

use super::{create_events, ledger_count, scalar_i64, schema_kind, temporary_connection};
use crate::migrations::catalog::{MIGRATIONS, Migration, MigrationStep};
use crate::migrations::runner::{migrate, migrate_catalog};

const TIGHTEN_EVENTS_STEPS: &[MigrationStep] = &[MigrationStep::Sql(
    "ALTER TABLE events ADD COLUMN required_value TEXT NOT NULL DEFAULT 'x'",
)];

#[tokio::test]
async fn later_migration_can_tighten_an_earlier_owned_table() {
    let (_directory, connection) = temporary_connection("later-table-tightening").await;
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
