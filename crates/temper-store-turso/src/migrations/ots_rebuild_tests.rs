use libsql::Builder;

use super::runner::migrate;

#[tokio::test]
async fn compact_legacy_check_constraint_fails_without_mutation() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join("compact-check.db"))
        .build()
        .await
        .expect("build compact-check database");
    let connection = database.connect().expect("connect compact-check database");
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
                created_at TEXT NOT NULL DEFAULT (datetime('now')),CHECK(length(data)>0))",
            (),
        )
        .await
        .expect("legacy OTS table with compact check");
    connection
        .execute(
            "INSERT INTO ots_trajectories (trajectory_id, tenant, agent_id, data)
             VALUES ('trajectory-check', 'tenant-a', 'agent-a', '{}')",
            (),
        )
        .await
        .expect("legacy OTS row");

    let error = migrate(&connection)
        .await
        .expect_err("compact CHECK syntax must prevent a destructive rebuild");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 5"), "{diagnostic}");
    assert!(diagnostic.contains("matching 'CHECK'"), "{diagnostic}");

    let table_sql = scalar_string(
        &connection,
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'ots_trajectories'",
    )
    .await;
    assert!(table_sql.contains(",CHECK(length(data)>0)"), "{table_sql}");
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM ots_trajectories WHERE trajectory_id = 'trajectory-check'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM pragma_table_info('ots_trajectories')
             WHERE name IN ('persistence_status', 'persist_attempts', 'last_error', 'updated_at')"
        )
        .await,
        0
    );
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM temper_schema_migrations").await,
        4
    );
}

#[tokio::test]
async fn current_shape_check_constraint_fails_without_ledgering_migration_five() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join("current-check.db"))
        .build()
        .await
        .expect("build current-check database");
    let connection = database.connect().expect("connect current-check database");
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
                persistence_status TEXT NOT NULL DEFAULT 'persisted',
                persist_attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                CHECK (data <> '{}')
            )",
            (),
        )
        .await
        .expect("current OTS table with restrictive check");
    connection
        .execute(
            "INSERT INTO ots_trajectories (trajectory_id, tenant, agent_id, data)
             VALUES ('trajectory-current-check', 'tenant-a', 'agent-a', '{\"turns\":1}')",
            (),
        )
        .await
        .expect("current OTS row");
    let before = scalar_string(
        &connection,
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'ots_trajectories'",
    )
    .await;

    let error = migrate(&connection)
        .await
        .expect_err("a current-shape CHECK must prevent migration readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 5"), "{diagnostic}");
    assert!(diagnostic.contains("matching 'CHECK'"), "{diagnostic}");
    assert_eq!(
        scalar_string(
            &connection,
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'ots_trajectories'",
        )
        .await,
        before
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM ots_trajectories
             WHERE trajectory_id = 'trajectory-current-check'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM temper_schema_migrations").await,
        4
    );
}

#[tokio::test]
async fn current_shape_short_generated_column_fails_without_mutation() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join("current-generated.db"))
        .build()
        .await
        .expect("build current-generated database");
    let connection = database
        .connect()
        .expect("connect current-generated database");
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
                persistence_status TEXT NOT NULL DEFAULT 'persisted',
                persist_attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                risky TEXT AS (json_extract(data, '$.required')) NOT NULL
            )",
            (),
        )
        .await
        .expect("current OTS table with short-form generated column");
    connection
        .execute(
            "INSERT INTO ots_trajectories (
                trajectory_id, tenant, agent_id, data
             ) VALUES (
                'trajectory-generated', 'tenant-a', 'agent-a', '{\"required\":\"present\"}'
             )",
            (),
        )
        .await
        .expect("current OTS row satisfying generated restriction");
    let before = scalar_string(
        &connection,
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'ots_trajectories'",
    )
    .await;

    let error = migrate(&connection)
        .await
        .expect_err("a current-shape short-form generated column must prevent readiness");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 5"), "{diagnostic}");
    assert!(diagnostic.contains("risky"), "{diagnostic}");
    assert_eq!(
        scalar_string(
            &connection,
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'ots_trajectories'",
        )
        .await,
        before
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM ots_trajectories
             WHERE trajectory_id = 'trajectory-generated'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM temper_schema_migrations").await,
        4
    );
}

#[tokio::test]
async fn descending_primary_key_fails_before_legacy_ots_rebuild() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join("descending-primary-key.db"))
        .build()
        .await
        .expect("build descending-primary-key database");
    let connection = database
        .connect()
        .expect("connect descending-primary-key database");
    create_legacy_ots(&connection, "PRIMARY KEY DESC").await;
    connection
        .execute(
            "INSERT INTO ots_trajectories (trajectory_id, tenant, agent_id, data)
             VALUES ('trajectory-desc', 'tenant-a', 'agent-a', '{}')",
            (),
        )
        .await
        .expect("legacy OTS row with descending primary key");
    let before = scalar_string(
        &connection,
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'ots_trajectories'",
    )
    .await;

    let error = migrate(&connection)
        .await
        .expect_err("descending primary-key semantics must prevent a rebuild");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 5"), "{diagnostic}");
    assert!(diagnostic.contains("primary key semantics"), "{diagnostic}");
    assert!(diagnostic.contains("descending: true"), "{diagnostic}");
    assert_eq!(
        scalar_string(
            &connection,
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'ots_trajectories'",
        )
        .await,
        before
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM ots_trajectories WHERE trajectory_id = 'trajectory-desc'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM temper_schema_migrations").await,
        4
    );
}

#[tokio::test]
async fn legal_sqlitex_child_foreign_key_fails_before_cascade_data_loss() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join("sqlitex-child.db"))
        .build()
        .await
        .expect("build sqliteX-child database");
    let connection = database.connect().expect("connect sqliteX-child database");
    connection
        .execute("PRAGMA foreign_keys = ON", ())
        .await
        .expect("enable foreign keys");
    create_legacy_ots(&connection, "PRIMARY KEY").await;
    connection
        .execute(
            "CREATE TABLE sqliteX_child (
                id TEXT PRIMARY KEY,
                trajectory_id TEXT NOT NULL REFERENCES ots_trajectories(trajectory_id)
                    ON DELETE CASCADE
            )",
            (),
        )
        .await
        .expect("legal child table with inbound OTS foreign key");
    connection
        .execute(
            "INSERT INTO ots_trajectories (trajectory_id, tenant, agent_id, data)
             VALUES ('trajectory-parent', 'tenant-a', 'agent-a', '{}')",
            (),
        )
        .await
        .expect("legacy OTS parent row");
    connection
        .execute(
            "INSERT INTO sqliteX_child (id, trajectory_id)
             VALUES ('child-1', 'trajectory-parent')",
            (),
        )
        .await
        .expect("inbound child row");
    let before = scalar_string(
        &connection,
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'ots_trajectories'",
    )
    .await;

    let error = migrate(&connection)
        .await
        .expect_err("an inbound FK from a legal sqliteX name must prevent a rebuild");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 5"), "{diagnostic}");
    assert!(diagnostic.contains("sqliteX_child"), "{diagnostic}");
    assert!(diagnostic.contains("inbound foreign key"), "{diagnostic}");
    assert_eq!(
        scalar_string(
            &connection,
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'ots_trajectories'",
        )
        .await,
        before
    );
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM sqliteX_child").await,
        1
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM ots_trajectories WHERE trajectory_id = 'trajectory-parent'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM temper_schema_migrations").await,
        4
    );
}

#[tokio::test]
async fn differently_cased_ots_trigger_owner_is_preserved_when_contract_rejects() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join("case-folded-ots-trigger.db"))
        .build()
        .await
        .expect("build case-folded-OTS-trigger database");
    let connection = database
        .connect()
        .expect("connect case-folded-OTS-trigger database");
    create_legacy_ots(&connection, "PRIMARY KEY").await;
    connection
        .execute(
            "CREATE TRIGGER reject_case_folded_ots BEFORE INSERT ON OTS_TRAJECTORIES
             BEGIN SELECT RAISE(FAIL, 'blocked'); END",
            (),
        )
        .await
        .expect("create blocking trigger with differently cased OTS owner");
    let before = scalar_string(
        &connection,
        "SELECT sql FROM sqlite_schema
         WHERE type = 'trigger' AND name = 'reject_case_folded_ots'",
    )
    .await;

    let error = migrate(&connection)
        .await
        .expect_err("the preserved trigger must fail the supported audit contract");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("migration 5"), "{diagnostic}");
    assert!(
        diagnostic.contains("reject_case_folded_ots"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("unsupported executable trigger extension"),
        "{diagnostic}"
    );
    assert_eq!(
        scalar_string(
            &connection,
            "SELECT sql FROM sqlite_schema
             WHERE type = 'trigger' AND name = 'reject_case_folded_ots'",
        )
        .await,
        before
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT COUNT(*) FROM pragma_table_info('ots_trajectories')
             WHERE name = 'updated_at'"
        )
        .await,
        0,
        "the rejected contract must roll back the destructive rebuild"
    );
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM temper_schema_migrations").await,
        4
    );
}

async fn create_legacy_ots(connection: &libsql::Connection, primary_key: &str) {
    let sql = format!(
        "CREATE TABLE ots_trajectories (
            trajectory_id TEXT {primary_key},
            tenant TEXT NOT NULL,
            agent_id TEXT NOT NULL,
            session_id TEXT,
            outcome TEXT NOT NULL DEFAULT 'unknown',
            entity_type TEXT,
            turn_count INTEGER NOT NULL DEFAULT 0,
            data TEXT NOT NULL,
            persistence_status TEXT NOT NULL DEFAULT 'persisted',
            persist_attempts INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        )"
    );
    connection
        .execute(&sql, ())
        .await
        .expect("create legacy OTS table");
}

async fn scalar_string(connection: &libsql::Connection, sql: &str) -> String {
    let mut rows = connection
        .query(sql, ())
        .await
        .expect("query string scalar");
    rows.next()
        .await
        .expect("read string scalar")
        .expect("string scalar row")
        .get::<String>(0)
        .expect("string scalar value")
}

async fn scalar_i64(connection: &libsql::Connection, sql: &str) -> i64 {
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
