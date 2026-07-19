use libsql::{Builder, Connection, params};

use super::catalog::MIGRATIONS;
use super::runner::migrate;
use crate::store::ots::PERSIST_OTS_TRAJECTORY_SQL;

#[tokio::test]
async fn current_ots_shape_preserves_harmless_inbound_reference() {
    let (_directory, connection) = current_ots_with_child("current-inbound", "NO ACTION").await;

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

#[tokio::test]
async fn current_ots_cascade_child_survives_existing_id_production_persist() {
    assert_existing_id_persist_preserves_child("current-cascade", "CASCADE").await;
}

#[tokio::test]
async fn current_ots_restrict_child_allows_existing_id_production_persist() {
    assert_existing_id_persist_preserves_child("current-restrict", "RESTRICT").await;
}

async fn assert_existing_id_persist_preserves_child(label: &str, on_delete: &str) {
    let (_directory, connection) = current_ots_with_child(label, on_delete).await;

    migrate(&connection)
        .await
        .expect("current OTS schema with an inbound reference remains compatible");
    connection
        .execute(
            PERSIST_OTS_TRAJECTORY_SQL,
            params![
                "trajectory-current".to_string(),
                "tenant-updated".to_string(),
                "agent-updated".to_string(),
                "session-updated".to_string(),
                "persisted-updated".to_string(),
                7_i64,
                "{\"stage\":\"updated\"}".to_string(),
            ],
        )
        .await
        .expect("existing-ID production persist must not delete or reject inbound references");

    assert_eq!(
        scalar_i64(&connection, "SELECT COUNT(*) FROM current_ots_child").await,
        1,
        "ON DELETE {on_delete} child must survive an existing-ID production persist"
    );
    assert_eq!(
        scalar_i64(
            &connection,
            "SELECT turn_count FROM ots_trajectories
             WHERE trajectory_id = 'trajectory-current'"
        )
        .await,
        7
    );
}

async fn current_ots_with_child(label: &str, on_delete: &str) -> (tempfile::TempDir, Connection) {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database = Builder::new_local(directory.path().join(format!("{label}.db")))
        .build()
        .await
        .expect("build current OTS database");
    let connection = database.connect().expect("connect current OTS database");
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
            &format!(
                "CREATE TABLE current_ots_child (
                id TEXT PRIMARY KEY,
                trajectory_id TEXT NOT NULL
                    REFERENCES ots_trajectories(trajectory_id) ON DELETE {on_delete}
            )"
            ),
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
    (directory, connection)
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
