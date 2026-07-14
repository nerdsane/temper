use libsql::Builder;

use super::catalog::MIGRATIONS;
use super::runner::migrate;

#[tokio::test]
async fn current_ots_shape_preserves_harmless_inbound_reference() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join("current-inbound.db"))
        .build()
        .await
        .expect("build current-inbound database");
    let connection = database
        .connect()
        .expect("connect current-inbound database");
    connection
        .execute("PRAGMA foreign_keys = ON", ())
        .await
        .expect("enable foreign keys");
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
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
            (),
        )
        .await
        .expect("create current OTS table");
    connection
        .execute(
            "CREATE TABLE current_ots_child (
                id TEXT PRIMARY KEY,
                trajectory_id TEXT NOT NULL REFERENCES ots_trajectories(trajectory_id)
            )",
            (),
        )
        .await
        .expect("create current OTS child table");
    connection
        .execute(
            "INSERT INTO ots_trajectories (trajectory_id, tenant, agent_id, data)
             VALUES ('trajectory-current', 'tenant-a', 'agent-a', '{}')",
            (),
        )
        .await
        .expect("insert current OTS parent");
    connection
        .execute(
            "INSERT INTO current_ots_child (id, trajectory_id)
             VALUES ('child-current', 'trajectory-current')",
            (),
        )
        .await
        .expect("insert current OTS child");

    migrate(&connection)
        .await
        .expect("an already-current table needs no destructive inbound-FK gate");

    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM ots_trajectories").await,
        1
    );
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM current_ots_child").await,
        1
    );
    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM temper_schema_migrations").await,
        MIGRATIONS.len() as i64
    );
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
        .expect("decode integer scalar")
}
