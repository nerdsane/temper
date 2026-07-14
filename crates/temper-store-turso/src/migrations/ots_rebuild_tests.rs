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
