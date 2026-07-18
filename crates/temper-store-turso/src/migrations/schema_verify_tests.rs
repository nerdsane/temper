use libsql::{Builder, Connection};

use super::catalog::MIGRATIONS;
use super::runner::migrate;

mod runtime_extensions;

#[tokio::test]
async fn unexpected_required_column_prevents_ledgering_and_preserves_table() {
    let (_directory, connection) = temporary_connection("required-column").await;
    create_events(&connection, Some("must_fill TEXT NOT NULL"), None).await;
    connection
        .execute(
            "INSERT INTO events (
                tenant, entity_type, entity_id, sequence_nr, event_type, payload, must_fill
             ) VALUES ('tenant-a', 'Order', 'order-1', 1, 'Created', '{}', 'present')",
            (),
        )
        .await
        .expect("seed restricted events table");
    let before = table_sql(&connection, "events").await;

    let error = migrate(&connection)
        .await
        .expect_err("an unexpected required column must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(diagnostic.contains("must_fill"), "{diagnostic}");
    assert!(
        diagnostic.contains("unexpected required column"),
        "{diagnostic}"
    );
    assert_eq!(table_sql(&connection, "events").await, before);
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM events").await,
        1
    );
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn unexpected_unique_index_prevents_ledgering_without_dropping_index() {
    let (_directory, connection) = temporary_connection("unique-index").await;
    create_events(&connection, None, None).await;
    connection
        .execute(
            "CREATE UNIQUE INDEX events_unexpected_unique
             ON events(tenant, event_type)",
            (),
        )
        .await
        .expect("unexpected unique index");

    let error = migrate(&connection)
        .await
        .expect_err("an unexpected unique restriction must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(
        diagnostic.contains("unique key restrictions"),
        "{diagnostic}"
    );
    assert_eq!(
        schema_kind(&connection, "events_unexpected_unique")
            .await
            .as_deref(),
        Some("index")
    );
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn unexpected_foreign_key_prevents_ledgering_and_preserves_table() {
    let (_directory, connection) = temporary_connection("foreign-key").await;
    connection
        .execute("CREATE TABLE tenant_guard (tenant TEXT PRIMARY KEY)", ())
        .await
        .expect("foreign-key target");
    create_events(
        &connection,
        None,
        Some("FOREIGN KEY (tenant) REFERENCES tenant_guard(tenant)"),
    )
    .await;
    let before = table_sql(&connection, "events").await;

    let error = migrate(&connection)
        .await
        .expect_err("an unexpected foreign-key restriction must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(
        diagnostic.contains("foreign key restrictions"),
        "{diagnostic}"
    );
    assert_eq!(table_sql(&connection, "events").await, before);
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn unexpected_check_semantics_prevent_ledgering_and_preserve_table() {
    let (_directory, connection) = temporary_connection("check-semantics").await;
    create_events(&connection, None, Some("CHECK (length(payload) > 0)")).await;
    let before = table_sql(&connection, "events").await;

    let error = migrate(&connection)
        .await
        .expect_err("an unmodeled CHECK restriction must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(
        diagnostic.contains("restricted table semantics"),
        "{diagnostic}"
    );
    assert_eq!(table_sql(&connection, "events").await, before);
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn nullable_extension_remains_compatible_with_runtime_inserts() {
    let (_directory, connection) = temporary_connection("nullable-extension").await;
    create_events(&connection, Some("deployment_note TEXT"), None).await;

    migrate(&connection)
        .await
        .expect("a nullable extension must remain compatible");
    connection
        .execute(
            "INSERT INTO events (
                tenant, entity_type, entity_id, sequence_nr, event_type, payload
             ) VALUES ('tenant-a', 'Order', 'order-1', 1, 'Created', '{}')",
            (),
        )
        .await
        .expect("canonical runtime insert with nullable extension");
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM events").await,
        1
    );
    assert_eq!(ledger_count(&connection).await, MIGRATIONS.len() as i64);
}

#[tokio::test]
async fn nullable_extension_with_executable_default_prevents_ledgering() {
    let (_directory, connection) = temporary_connection("unsafe-nullable-default").await;
    create_events(
        &connection,
        Some("risky TEXT DEFAULT (json_extract('bad', '$'))"),
        None,
    )
    .await;
    let before = table_sql(&connection, "events").await;

    let error = migrate(&connection)
        .await
        .expect_err("an executable default must not be treated as omission-safe");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(diagnostic.contains("risky"), "{diagnostic}");
    assert_eq!(table_sql(&connection, "events").await, before);
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn short_form_generated_column_prevents_ledgering() {
    let (_directory, connection) = temporary_connection("short-generated").await;
    create_events(
        &connection,
        Some("risky TEXT AS (json_extract(payload, '$.required')) NOT NULL"),
        None,
    )
    .await;
    let before = table_sql(&connection, "events").await;

    let error = migrate(&connection)
        .await
        .expect_err("a short-form generated restriction must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(diagnostic.contains("risky"), "{diagnostic}");
    assert!(diagnostic.contains("hidden: 2"), "{diagnostic}");
    assert_eq!(table_sql(&connection, "events").await, before);
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn partial_unique_index_cannot_replace_required_full_unique_key() {
    let (_directory, connection) = temporary_connection("partial-unique").await;
    create_events_with_identity_unique(&connection, None, None, false).await;
    connection
        .execute(
            "CREATE UNIQUE INDEX events_partial_identity
             ON events(tenant, entity_type, entity_id, sequence_nr)
             WHERE length(tenant) > 0",
            (),
        )
        .await
        .expect("partial identity index");

    let error = migrate(&connection)
        .await
        .expect_err("a partial index must not satisfy a required full unique key");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(
        diagnostic.contains("unique key restrictions"),
        "{diagnostic}"
    );
    assert_eq!(
        schema_kind(&connection, "events_partial_identity")
            .await
            .as_deref(),
        Some("index")
    );
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn generated_column_matching_add_step_reports_semantic_incompatibility() {
    let (_directory, connection) = temporary_connection("generated-add-step").await;
    create_events(
        &connection,
        Some("segment_index INTEGER AS (sequence_nr - 1)"),
        None,
    )
    .await;
    let before = table_sql(&connection, "events").await;

    let error = migrate(&connection)
        .await
        .expect_err("a generated AddColumn target must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(diagnostic.contains("segment_index"), "{diagnostic}");
    assert!(diagnostic.contains("hidden: 2"), "{diagnostic}");
    assert!(!diagnostic.contains("duplicate column"), "{diagnostic}");
    assert_eq!(table_sql(&connection, "events").await, before);
    assert_eq!(ledger_count(&connection).await, 0);
}

#[tokio::test]
async fn equivalent_lowercase_partial_predicate_remains_compatible() {
    let (_directory, connection) = temporary_connection("lowercase-partial-predicate").await;
    migrate(&connection).await.expect("install catalog");
    connection
        .execute("DROP INDEX idx_blobs_expires_at", ())
        .await
        .expect("drop canonical partial index");
    connection
        .execute(
            "CREATE INDEX idx_blobs_expires_at
             ON blobs(expires_at) where expires_at is not null",
            (),
        )
        .await
        .expect("create equivalent lowercase partial index");

    migrate(&connection)
        .await
        .expect("keyword case must not make an equivalent predicate incompatible");
    assert_eq!(ledger_count(&connection).await, MIGRATIONS.len() as i64);
}

async fn create_events(
    connection: &Connection,
    extra_column: Option<&str>,
    extra_constraint: Option<&str>,
) {
    create_events_with_identity_unique(connection, extra_column, extra_constraint, true).await;
}

async fn create_events_with_identity_unique(
    connection: &Connection,
    extra_column: Option<&str>,
    extra_constraint: Option<&str>,
    include_identity_unique: bool,
) {
    let mut definitions = vec![
        "id INTEGER PRIMARY KEY AUTOINCREMENT",
        "tenant TEXT NOT NULL",
        "entity_type TEXT NOT NULL",
        "entity_id TEXT NOT NULL",
        "sequence_nr INTEGER NOT NULL",
        "event_type TEXT NOT NULL",
        "payload TEXT NOT NULL",
        "metadata TEXT",
        "created_at TEXT NOT NULL DEFAULT (datetime('now'))",
    ];
    if let Some(extra_column) = extra_column {
        definitions.push(extra_column);
    }
    if include_identity_unique {
        definitions.push("UNIQUE(tenant, entity_type, entity_id, sequence_nr)");
    }
    if let Some(extra_constraint) = extra_constraint {
        definitions.push(extra_constraint);
    }
    let sql = format!("CREATE TABLE events ({})", definitions.join(", "));
    connection
        .execute(&sql, ())
        .await
        .expect("create legacy events table");
}

async fn temporary_connection(label: &str) -> (tempfile::TempDir, Connection) {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join(format!("{label}.db")))
        .build()
        .await
        .expect("build temporary database");
    let connection = database.connect().expect("connect temporary database");
    (directory, connection)
}

async fn table_sql(connection: &Connection, table: &str) -> String {
    let mut rows = connection
        .query(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [table],
        )
        .await
        .expect("query table SQL");
    rows.next()
        .await
        .expect("read table SQL")
        .expect("table SQL row")
        .get::<String>(0)
        .expect("decode table SQL")
}

async fn schema_kind(connection: &Connection, name: &str) -> Option<String> {
    let mut rows = connection
        .query(
            "SELECT type FROM sqlite_schema WHERE name = ?1 ORDER BY type LIMIT 1",
            [name],
        )
        .await
        .expect("query schema kind");
    rows.next()
        .await
        .expect("read schema kind")
        .map(|row| row.get::<String>(0).expect("decode schema kind"))
}

async fn ledger_count(connection: &Connection) -> i64 {
    scalar_i64(connection, "SELECT COUNT(*) FROM temper_schema_migrations").await
}

async fn scalar_i64(connection: &Connection, sql: &str) -> i64 {
    let mut rows = connection
        .query(sql, ())
        .await
        .expect("query integer scalar");
    rows.next()
        .await
        .expect("read integer scalar")
        .expect("integer scalar row")
        .get::<i64>(0)
        .expect("integer scalar value")
}
