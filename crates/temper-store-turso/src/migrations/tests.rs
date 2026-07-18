use std::process::{Command, Stdio};

use libsql::{Builder, Connection, params};

use super::catalog::MIGRATIONS;
use super::runner::{FaultInjection, expected_checksums, migrate, migrate_prefix};
use crate::TursoEventStore;

#[tokio::test]
async fn migration_catalog_is_contiguous_and_checksummed() {
    let checksums = expected_checksums().await.expect("expected checksums");
    for (index, migration) in MIGRATIONS.iter().enumerate() {
        assert_eq!(migration.version, (index + 1) as u32);
        assert_eq!(checksums[index].len(), 64);
    }
}

#[tokio::test]
async fn fresh_install_records_every_migration_and_duplicate_replay_is_stable() {
    let (_directory, connection) = temporary_connection("fresh").await;
    migrate(&connection).await.expect("fresh migration");
    let before = ledger_rows(&connection).await;
    let checksums = expected_checksums().await.expect("expected checksums");
    assert_eq!(before.len(), MIGRATIONS.len());
    for ((row, migration), checksum) in before.iter().zip(MIGRATIONS).zip(checksums) {
        assert_eq!(row.0, migration.version as i64);
        assert_eq!(row.1, migration.name);
        assert_eq!(row.2, checksum);
        assert_eq!(row.2.len(), 64);
    }
    migrate(&connection).await.expect("idempotent replay");
    assert_eq!(ledger_rows(&connection).await, before);
}

#[tokio::test]
async fn every_supported_version_prefix_upgrades_to_latest() {
    for prefix in 0..=MIGRATIONS.len() {
        let (_directory, connection) = temporary_connection(&format!("prefix-{prefix}")).await;
        migrate_prefix(&connection, prefix, FaultInjection::default())
            .await
            .unwrap_or_else(|error| panic!("install prefix {prefix}: {error}"));
        migrate(&connection)
            .await
            .unwrap_or_else(|error| panic!("upgrade prefix {prefix}: {error}"));
        assert_eq!(ledger_rows(&connection).await.len(), MIGRATIONS.len());
    }
}

#[tokio::test]
async fn legacy_partial_schema_is_reconciled_without_losing_rows() {
    let (_directory, connection) = temporary_connection("legacy-partial").await;
    connection
        .execute(
            "CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                tenant TEXT NOT NULL,
                entity_type TEXT NOT NULL,
                entity_id TEXT NOT NULL,
                sequence_nr INTEGER NOT NULL,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL,
                metadata TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                UNIQUE(tenant, entity_type, entity_id, sequence_nr)
            )",
            (),
        )
        .await
        .expect("legacy events table");
    install_legacy_ots_table(&connection).await;
    connection
        .execute(
            "CREATE TABLE ots_insert_audit (trajectory_id TEXT PRIMARY KEY)",
            (),
        )
        .await
        .expect("legacy OTS audit table");
    connection
        .execute(
            "CREATE INDEX idx_ots_legacy_session ON ots_trajectories(session_id)",
            (),
        )
        .await
        .expect("legacy OTS custom index");
    connection
        .execute(
            "CREATE TRIGGER trg_ots_legacy_insert AFTER INSERT ON ots_trajectories
             BEGIN INSERT INTO ots_insert_audit (trajectory_id) VALUES (NEW.trajectory_id); END",
            (),
        )
        .await
        .expect("legacy OTS custom trigger");
    connection
        .execute(
            "INSERT INTO ots_trajectories (
                trajectory_id, tenant, agent_id, data, created_at
             ) VALUES ('trajectory-1', 'tenant-a', 'agent-a', '{\"turns\":1}', '2026-07-14T00:00:00Z')",
            (),
        )
        .await
        .expect("legacy OTS row");
    migrate(&connection).await.expect("legacy reconciliation");
    assert!(
        column_names(&connection, "events")
            .await
            .iter()
            .any(|column| column == "segment_index")
    );
    let mut rows = connection
        .query(
            "SELECT data, persistence_status, persist_attempts, last_error, updated_at
             FROM ots_trajectories WHERE trajectory_id = 'trajectory-1'",
            (),
        )
        .await
        .expect("query reconciled OTS row");
    let row = rows
        .next()
        .await
        .expect("read reconciled OTS row")
        .expect("reconciled OTS row exists");
    assert_eq!(row.get::<String>(0).expect("data"), "{\"turns\":1}");
    assert_eq!(
        row.get::<String>(1).expect("persistence status"),
        "persisted"
    );
    assert_eq!(row.get::<i64>(2).expect("persist attempts"), 0);
    assert_eq!(row.get::<Option<String>>(3).expect("last error"), None);
    assert_eq!(
        row.get::<String>(4).expect("updated at"),
        "2026-07-14T00:00:00Z"
    );
    assert_eq!(
        schema_kind(&connection, "idx_ots_legacy_session")
            .await
            .as_deref(),
        Some("index")
    );
    assert_eq!(
        schema_kind(&connection, "trg_ots_legacy_insert")
            .await
            .as_deref(),
        Some("trigger")
    );
    connection
        .execute(
            "INSERT INTO ots_trajectories (trajectory_id, tenant, agent_id, data)
             VALUES ('trajectory-2', 'tenant-a', 'agent-a', '{}')",
            (),
        )
        .await
        .expect("insert through preserved OTS trigger");
    assert_eq!(
        scalar_count(
            &connection,
            "SELECT COUNT(*) FROM ots_insert_audit WHERE trajectory_id = 'trajectory-2'"
        )
        .await,
        1
    );
    assert_eq!(ledger_rows(&connection).await.len(), MIGRATIONS.len());
}

#[tokio::test]
async fn legacy_ots_extra_column_fails_closed_without_mutation() {
    let (_directory, connection) = temporary_connection("legacy-ots-extra").await;
    install_legacy_ots_table(&connection).await;
    connection
        .execute(
            "ALTER TABLE ots_trajectories ADD COLUMN deployment_note TEXT",
            (),
        )
        .await
        .expect("legacy custom OTS column");
    connection
        .execute(
            "INSERT INTO ots_trajectories (trajectory_id, tenant, agent_id, data, deployment_note)
             VALUES ('trajectory-extra', 'tenant-a', 'agent-a', '{}', 'preserve-me')",
            (),
        )
        .await
        .expect("legacy custom OTS row");
    let error = migrate(&connection)
        .await
        .expect_err("an unmodeled legacy column must prevent destructive reconciliation");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 5"), "{diagnostic}");
    assert!(
        diagnostic.contains("exactly the pre-upgrade columns"),
        "{diagnostic}"
    );
    assert!(
        column_names(&connection, "ots_trajectories")
            .await
            .iter()
            .any(|column| column == "deployment_note")
    );
    assert_eq!(
        scalar_count(
            &connection,
            "SELECT COUNT(*) FROM ots_trajectories
             WHERE trajectory_id = 'trajectory-extra' AND deployment_note = 'preserve-me'"
        )
        .await,
        1
    );
    assert_eq!(ledger_rows(&connection).await.len(), 4);
}

#[tokio::test]
async fn interrupted_migration_rolls_back_schema_and_ledger_then_retries() {
    let (_directory, connection) = temporary_connection("interrupted").await;
    let error = migrate_prefix(
        &connection,
        1,
        FaultInjection {
            after_step: Some((1, 0)),
            ..FaultInjection::default()
        },
    )
    .await
    .expect_err("fault injection must interrupt migration");
    assert!(
        error
            .to_string()
            .contains("injected migration interruption")
    );
    assert_eq!(schema_kind(&connection, "events").await, None);
    assert!(ledger_rows(&connection).await.is_empty());

    migrate(&connection).await.expect("retry after rollback");
    assert_eq!(
        schema_kind(&connection, "events").await.as_deref(),
        Some("table")
    );
    assert_eq!(ledger_rows(&connection).await.len(), MIGRATIONS.len());
}

#[tokio::test]
async fn backend_ddl_error_rolls_back_active_version_with_context() {
    let (_directory, connection) = temporary_connection("ddl-error").await;
    let error = migrate_prefix(
        &connection,
        1,
        FaultInjection {
            ddl_error_at: Some((1, 1)),
            ..FaultInjection::default()
        },
    )
    .await
    .expect_err("an actual libSQL DDL error must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 1"), "{diagnostic}");
    assert!(
        diagnostic.contains("event-journal-and-snapshots"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("step 1"), "{diagnostic}");
    assert!(
        diagnostic.contains("__temper_injected_missing_table"),
        "{diagnostic}"
    );
    assert_eq!(schema_kind(&connection, "events").await, None);
    assert!(ledger_rows(&connection).await.is_empty());

    migrate(&connection)
        .await
        .expect("retry after backend DDL failure");
    assert_eq!(ledger_rows(&connection).await.len(), MIGRATIONS.len());
}

#[tokio::test]
async fn checksum_gap_and_incompatible_newer_versions_fail_closed() {
    let (_directory, connection) = temporary_connection("ledger-corruption").await;
    migrate(&connection).await.expect("initial migration");
    let checksums = expected_checksums().await.expect("expected checksums");

    connection
        .execute(
            "UPDATE temper_schema_migrations SET checksum = ?1 WHERE version = 1",
            ["0".repeat(64)],
        )
        .await
        .expect("alter checksum");
    let error = migrate(&connection)
        .await
        .expect_err("checksum drift must fail startup");
    assert!(error.to_string().contains("checksum mismatch"));
    connection
        .execute(
            "UPDATE temper_schema_migrations SET checksum = ?1 WHERE version = 1",
            [checksums[0].as_str()],
        )
        .await
        .expect("restore checksum");

    connection
        .execute("DELETE FROM temper_schema_migrations WHERE version = 3", ())
        .await
        .expect("create ledger gap");
    let error = migrate(&connection)
        .await
        .expect_err("ledger gap must fail startup");
    assert!(error.to_string().contains("version gap"));
    connection
        .execute(
            "INSERT INTO temper_schema_migrations (version, name, checksum)
             VALUES (?1, ?2, ?3)",
            params![
                MIGRATIONS[2].version as i64,
                MIGRATIONS[2].name,
                checksums[2].as_str()
            ],
        )
        .await
        .expect("restore missing ledger row");

    connection
        .execute(
            "INSERT INTO temper_schema_migrations (version, name, checksum)
             VALUES (8, 'future-schema', ?1)",
            ["f".repeat(64)],
        )
        .await
        .expect("install newer ledger row");
    let error = migrate(&connection)
        .await
        .expect_err("newer schema must fail startup");
    assert!(
        error
            .to_string()
            .contains("newer than this binary supports")
    );
}

#[tokio::test]
async fn semantic_index_drift_prevents_readiness() {
    let (_directory, connection) = temporary_connection("semantic-drift").await;
    migrate(&connection).await.expect("initial migration");
    connection
        .execute("DROP INDEX idx_events_entity", ())
        .await
        .expect("drop expected index");
    connection
        .execute("CREATE INDEX idx_events_entity ON events(event_type)", ())
        .await
        .expect("install incompatible index");
    let error = migrate(&connection)
        .await
        .expect_err("semantic index drift must fail readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("idx_events_entity"), "{diagnostic}");
    assert!(
        diagnostic.contains("incompatible semantics"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("migration 7"), "{diagnostic}");
}

#[test]
fn independent_startup_processes_converge_on_one_ledger() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let executable = std::env::current_exe().expect("current test executable");
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    for round in 0..8 {
        let database_path = directory.path().join(format!("multiprocess-{round}.db"));
        let url = format!("file:{}", database_path.display());
        let spawn_child = || {
            Command::new(&executable)
                .args([
                    "--ignored",
                    "--exact",
                    "migrations::tests::migration_process_child",
                    "--nocapture",
                ])
                .env("TEMPER_MIGRATION_TEST_URL", &url)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn migration child")
        };
        let first = spawn_child();
        let second = spawn_child();
        let first_output = first.wait_with_output().expect("wait for first child");
        let second_output = second.wait_with_output().expect("wait for second child");
        assert_child_succeeded("first", &first_output);
        assert_child_succeeded("second", &second_output);
        runtime.block_on(async {
            let database = Builder::new_local(&database_path)
                .build()
                .await
                .expect("open migrated database");
            let connection = database.connect().expect("connect to migrated database");
            assert_eq!(ledger_rows(&connection).await.len(), MIGRATIONS.len());
        });
    }
}

#[tokio::test]
#[ignore = "helper process invoked by independent_startup_processes_converge_on_one_ledger"]
async fn migration_process_child() {
    let url = std::env::var("TEMPER_MIGRATION_TEST_URL").expect("migration child URL");
    TursoEventStore::new(&url, None)
        .await
        .expect("independent store startup");
}

async fn install_legacy_ots_table(connection: &Connection) {
    connection
        .execute(
            "CREATE TABLE ots_trajectories (
                trajectory_id TEXT PRIMARY KEY,
                tenant TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                session_id TEXT,
                outcome TEXT NOT NULL DEFAULT 'unknown',
                entity_type TEXT,
                turn_count INTEGER NOT NULL DEFAULT 0,
                data TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
            (),
        )
        .await
        .expect("legacy OTS table");
}

async fn temporary_connection(label: &str) -> (tempfile::TempDir, Connection) {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database_path = directory.path().join(format!("{label}.db"));
    let database = Builder::new_local(database_path)
        .build()
        .await
        .expect("create temporary database");
    let connection = database.connect().expect("connect to temporary database");
    (directory, connection)
}

async fn ledger_rows(connection: &Connection) -> Vec<(i64, String, String, String)> {
    let mut rows = connection
        .query(
            "SELECT version, name, checksum, applied_at
             FROM temper_schema_migrations ORDER BY version",
            (),
        )
        .await
        .expect("query migration ledger");
    let mut values = Vec::new();
    while let Some(row) = rows.next().await.expect("read migration ledger") {
        values.push((
            row.get::<i64>(0).expect("version"),
            row.get::<String>(1).expect("name"),
            row.get::<String>(2).expect("checksum"),
            row.get::<String>(3).expect("applied at"),
        ));
    }
    values
}

async fn scalar_count(connection: &Connection, sql: &str) -> i64 {
    let mut rows = connection.query(sql, ()).await.expect("query scalar count");
    rows.next()
        .await
        .expect("read scalar count")
        .expect("scalar count row")
        .get::<i64>(0)
        .expect("scalar count value")
}

async fn column_names(connection: &Connection, table: &str) -> Vec<String> {
    let mut rows = connection
        .query(&format!("PRAGMA table_info(\"{table}\")"), ())
        .await
        .expect("query table columns");
    let mut names = Vec::new();
    while let Some(row) = rows.next().await.expect("read table column") {
        names.push(row.get::<String>(1).expect("column name"));
    }
    names
}

async fn schema_kind(connection: &Connection, name: &str) -> Option<String> {
    let mut rows = connection
        .query("SELECT type FROM sqlite_schema WHERE name = ?1", [name])
        .await
        .expect("query schema kind");
    rows.next()
        .await
        .expect("read schema kind")
        .map(|row| row.get::<String>(0).expect("schema kind"))
}

fn assert_child_succeeded(label: &str, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{label} migration child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
