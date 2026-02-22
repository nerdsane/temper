//! Turso/libSQL-backed implementation of the [`EventStore`] trait.

use std::sync::Arc;

use libsql::{Builder, Database, TransactionBehavior, params};
use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceEnvelope, PersistenceError,
};

use crate::schema;

#[derive(Clone, Debug)]
pub struct TursoEventStore {
    db: Arc<Database>,
}

impl TursoEventStore {
    /// Connect to a Turso database.
    ///
    /// `url`: `"libsql://your-db.turso.io"` or `"file:local.db"` for local SQLite.
    /// `auth_token`: Turso auth token (`None` for local SQLite).
    pub async fn new(url: &str, auth_token: Option<&str>) -> Result<Self, PersistenceError> {
        let db = if url.starts_with("libsql://") {
            let token = auth_token.ok_or_else(|| {
                PersistenceError::Storage("auth token is required for libsql:// URLs".to_string())
            })?;
            Builder::new_remote(url.to_string(), token.to_string())
                .build()
                .await
                .map_err(storage_error)?
        } else {
            let local_path = url.strip_prefix("file:").unwrap_or(url);
            Builder::new_local(local_path)
                .build()
                .await
                .map_err(storage_error)?
        };

        let store = Self { db: Arc::new(db) };
        store.migrate().await?;
        Ok(store)
    }

    /// Obtain a connection configured for local-SQLite concurrency.
    ///
    /// WAL mode is set in `migrate()` (persists in the DB file). `busy_timeout`
    /// is a per-connection setting — 30 s gives concurrent verification threads
    /// time to wait for the write lock instead of immediately returning SQLITE_BUSY.
    async fn configured_connection(&self) -> Result<libsql::Connection, PersistenceError> {
        let conn = self.db.connect().map_err(storage_error)?;
        // busy_timeout returns the old value as a row — use query() and drop it.
        let _ = conn
            .query("PRAGMA busy_timeout=30000", ())
            .await
            .map_err(storage_error)?;
        Ok(conn)
    }

    /// Run schema migrations on connect.
    async fn migrate(&self) -> Result<(), PersistenceError> {
        let conn = self.connection()?;

        // WAL journal mode lets concurrent readers proceed while a writer holds the
        // lock, and allows multiple writers to serialise without SQLITE_BUSY errors
        // (combined with busy_timeout). The setting persists in the DB file.
        //
        // Both PRAGMAs return a row — use query() and drop the result set.
        let _ = conn
            .query("PRAGMA journal_mode=WAL", ())
            .await
            .map_err(storage_error)?;
        let _ = conn
            .query("PRAGMA busy_timeout=30000", ())
            .await
            .map_err(storage_error)?;

        conn.execute(schema::CREATE_EVENTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVENTS_ENTITY_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_SNAPSHOTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_SPECS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TRAJECTORIES_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TRAJECTORIES_SUCCESS_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TRAJECTORIES_ENTITY_ACTION_INDEX, ())
            .await
            .map_err(storage_error)?;

        Ok(())
    }

    /// Upsert a spec source (IOA + CSDL) for a tenant/entity_type.
    pub async fn upsert_spec(
        &self,
        tenant: &str,
        entity_type: &str,
        ioa_source: &str,
        csdl_xml: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO specs (tenant, entity_type, ioa_source, csdl_xml, version, verified, verification_status, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, 0, 'pending', datetime('now'))
             ON CONFLICT (tenant, entity_type) DO UPDATE SET
                 ioa_source = excluded.ioa_source,
                 csdl_xml = excluded.csdl_xml,
                 version = specs.version + 1,
                 verified = 0,
                 verification_status = 'pending',
                 levels_passed = NULL,
                 levels_total = NULL,
                 verification_result = NULL,
                 updated_at = datetime('now')",
            params![tenant, entity_type, ioa_source, csdl_xml],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Persist verification result for a spec.
    pub async fn persist_spec_verification(
        &self,
        tenant: &str,
        entity_type: &str,
        status: &str,
        verified: bool,
        levels_passed: Option<i32>,
        levels_total: Option<i32>,
        verification_result_json: Option<&str>,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "UPDATE specs SET
                 verified = ?3,
                 verification_status = ?4,
                 levels_passed = ?5,
                 levels_total = ?6,
                 verification_result = ?7,
                 updated_at = datetime('now')
             WHERE tenant = ?1 AND entity_type = ?2",
            params![
                tenant,
                entity_type,
                verified as i64,
                status,
                levels_passed,
                levels_total,
                verification_result_json
            ],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load all persisted specs (for startup recovery).
    pub async fn load_specs(&self) -> Result<Vec<TursoSpecRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, ioa_source, csdl_xml, verification_status, verified, \
                        levels_passed, levels_total, verification_result, updated_at \
                 FROM specs \
                 ORDER BY tenant, entity_type",
                (),
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TursoSpecRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                entity_type: row.get::<String>(1).map_err(storage_error)?,
                ioa_source: row.get::<String>(2).map_err(storage_error)?,
                csdl_xml: row.get::<Option<String>>(3).map_err(storage_error)?,
                verification_status: row.get::<String>(4).map_err(storage_error)?,
                verified: row.get::<i64>(5).map_err(storage_error)? != 0,
                levels_passed: row
                    .get::<Option<i64>>(6)
                    .map_err(storage_error)?
                    .map(|v| v as i32),
                levels_total: row
                    .get::<Option<i64>>(7)
                    .map_err(storage_error)?
                    .map(|v| v as i32),
                verification_result: row.get::<Option<String>>(8).map_err(storage_error)?,
                updated_at: row.get::<String>(9).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Persist a trajectory entry.
    pub async fn persist_trajectory(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        success: bool,
        from_status: Option<&str>,
        to_status: Option<&str>,
        error: Option<&str>,
        created_at: &str,
    ) -> Result<(), PersistenceError> {
        let conn = self.configured_connection().await?;
        conn.execute(
            "INSERT INTO trajectories \
             (tenant, entity_type, entity_id, action, success, from_status, to_status, error, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                tenant,
                entity_type,
                entity_id,
                action,
                success as i64,
                from_status,
                to_status,
                error,
                created_at
            ],
        )
        .await
        .map_err(storage_error)?;
        Ok(())
    }

    /// Load recent trajectory entries (newest first, up to `limit`).
    pub async fn load_recent_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT tenant, entity_type, entity_id, action, success, from_status, to_status, error, created_at \
                 FROM trajectories \
                 ORDER BY created_at DESC \
                 LIMIT ?1",
                params![limit],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            out.push(TursoTrajectoryRow {
                tenant: row.get::<String>(0).map_err(storage_error)?,
                entity_type: row.get::<String>(1).map_err(storage_error)?,
                entity_id: row.get::<String>(2).map_err(storage_error)?,
                action: row.get::<String>(3).map_err(storage_error)?,
                success: row.get::<i64>(4).map_err(storage_error)? != 0,
                from_status: row.get::<Option<String>>(5).map_err(storage_error)?,
                to_status: row.get::<Option<String>>(6).map_err(storage_error)?,
                error: row.get::<Option<String>>(7).map_err(storage_error)?,
                created_at: row.get::<String>(8).map_err(storage_error)?,
            });
        }
        Ok(out)
    }

    /// Obtain a connection handle to the underlying database.
    ///
    /// `Database::connect()` returns a lightweight handle, **not** a fresh TCP
    /// connection each time:
    /// - **Local SQLite** (`file:` URLs): a handle to the same underlying
    ///   database file — no network overhead.
    /// - **Remote Turso** (`libsql://` URLs): a handle drawn from an internal
    ///   HTTP/gRPC connection pool managed by the `libsql` crate.
    ///
    /// It is safe (and cheap) to call this at the start of every method.
    fn connection(&self) -> Result<libsql::Connection, PersistenceError> {
        self.db.connect().map_err(storage_error)
    }
}

fn storage_error(err: impl std::fmt::Display) -> PersistenceError {
    PersistenceError::Storage(err.to_string())
}

/// Split a persistence ID into `(tenant, entity_type, entity_id)`.
///
/// Accepts both the new 3-segment format (`tenant:type:id`) and the legacy
/// 2-segment format (`type:id`) mapped to the `"default"` tenant.
fn parse_persistence_id(persistence_id: &str) -> Result<(&str, &str, &str), PersistenceError> {
    let parts: Vec<&str> = persistence_id.splitn(3, ':').collect();
    match parts.len() {
        3 => {
            let tenant = parts[0];
            let entity_type = parts[1];
            let entity_id = parts[2];
            if tenant.is_empty() || entity_type.is_empty() || entity_id.is_empty() {
                return Err(PersistenceError::Storage(format!(
                    "invalid persistence_id (empty segment): {persistence_id}"
                )));
            }
            Ok((tenant, entity_type, entity_id))
        }
        2 => {
            let entity_type = parts[0];
            let entity_id = parts[1];
            if entity_type.is_empty() || entity_id.is_empty() {
                return Err(PersistenceError::Storage(format!(
                    "invalid persistence_id (empty segment): {persistence_id}"
                )));
            }
            Ok(("default", entity_type, entity_id))
        }
        _ => Err(PersistenceError::Storage(format!(
            "invalid persistence_id (expected 'tenant:type:id' or 'type:id'): {persistence_id}"
        ))),
    }
}

impl EventStore for TursoEventStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;
        let conn = self.configured_connection().await?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .await
            .map_err(storage_error)?;

        let mut rows = tx
            .query(
                "SELECT COALESCE(MAX(sequence_nr), 0)
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;

        let current_seq = match rows.next().await.map_err(storage_error)? {
            Some(row) => row.get::<i64>(0).map_err(storage_error)? as u64,
            None => 0,
        };
        drop(rows);

        if current_seq != expected_sequence {
            let _ = tx.rollback().await;
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: current_seq,
            });
        }

        let mut new_seq = expected_sequence;
        for event in events {
            new_seq += 1;
            let payload_json = serde_json::to_string(&event.payload)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            let metadata_json = serde_json::to_string(&event.metadata)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;

            let insert_result = tx
                .execute(
                    "INSERT INTO events
                     (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        tenant,
                        entity_type,
                        entity_id,
                        new_seq as i64,
                        event.event_type.as_str(),
                        payload_json,
                        metadata_json
                    ],
                )
                .await;

            if let Err(e) = insert_result {
                let msg = e.to_string();
                let _ = tx.rollback().await;
                if msg.contains("UNIQUE constraint failed") || msg.contains("UNIQUE") {
                    return Err(PersistenceError::ConcurrencyViolation {
                        expected: expected_sequence,
                        actual: new_seq,
                    });
                }
                return Err(PersistenceError::Storage(msg));
            }
        }

        tx.commit().await.map_err(storage_error)?;
        Ok(new_seq)
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;
        let conn = self.configured_connection().await?;

        let mut rows = conn
            .query(
                "SELECT sequence_nr, event_type, payload, metadata
                 FROM events
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND sequence_nr > ?4
                 ORDER BY sequence_nr ASC",
                params![tenant, entity_type, entity_id, from_sequence as i64],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let seq = row.get::<i64>(0).map_err(storage_error)? as u64;
            let event_type = row.get::<String>(1).map_err(storage_error)?;
            let payload_json = row.get::<String>(2).map_err(storage_error)?;
            let metadata_json = row.get::<Option<String>>(3).map_err(storage_error)?;

            let payload = serde_json::from_str(&payload_json)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
            let metadata_raw = metadata_json.ok_or_else(|| {
                PersistenceError::Serialization("missing event metadata".to_string())
            })?;
            let metadata: EventMetadata = serde_json::from_str(&metadata_raw)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;

            out.push(PersistenceEnvelope {
                sequence_nr: seq,
                event_type,
                payload,
                metadata,
            });
        }

        Ok(out)
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;
        let conn = self.configured_connection().await?;

        conn.execute(
            "INSERT INTO snapshots (tenant, entity_type, entity_id, sequence_nr, snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (tenant, entity_type, entity_id)
             DO UPDATE SET
                sequence_nr = excluded.sequence_nr,
                snapshot = excluded.snapshot,
                created_at = datetime('now')",
            params![
                tenant,
                entity_type,
                entity_id,
                sequence_nr as i64,
                snapshot.to_vec()
            ],
        )
        .await
        .map_err(storage_error)?;

        Ok(())
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT sequence_nr, snapshot
                 FROM snapshots
                 WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3
                 ORDER BY sequence_nr DESC
                 LIMIT 1",
                params![tenant, entity_type, entity_id],
            )
            .await
            .map_err(storage_error)?;

        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(None);
        };

        let sequence_nr = row.get::<i64>(0).map_err(storage_error)? as u64;
        let snapshot = row.get::<Vec<u8>>(1).map_err(storage_error)?;
        Ok(Some((sequence_nr, snapshot)))
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let mut rows = conn
            .query(
                "SELECT DISTINCT entity_type, entity_id
                 FROM events
                 WHERE tenant = ?1",
                params![tenant],
            )
            .await
            .map_err(storage_error)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_type = row.get::<String>(0).map_err(storage_error)?;
            let entity_id = row.get::<String>(1).map_err(storage_error)?;
            out.push((entity_type, entity_id));
        }
        Ok(out)
    }
}

/// Row returned by [`TursoEventStore::load_specs()`].
#[derive(Debug, Clone)]
pub struct TursoSpecRow {
    /// Tenant name.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// IOA TOML source.
    pub ioa_source: String,
    /// CSDL XML (may be absent for old rows).
    pub csdl_xml: Option<String>,
    /// Verification status string (pending/running/passed/failed/partial).
    pub verification_status: String,
    /// Whether the spec has been verified.
    pub verified: bool,
    /// Number of verification levels that passed.
    pub levels_passed: Option<i32>,
    /// Total number of verification levels.
    pub levels_total: Option<i32>,
    /// Serialized verification result JSON.
    pub verification_result: Option<String>,
    /// ISO-8601 updated_at timestamp.
    pub updated_at: String,
}

/// Row returned by [`TursoEventStore::load_recent_trajectories()`].
#[derive(Debug, Clone)]
pub struct TursoTrajectoryRow {
    /// Tenant name.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// Entity ID.
    pub entity_id: String,
    /// Action name.
    pub action: String,
    /// Whether the action succeeded.
    pub success: bool,
    /// Status before the action.
    pub from_status: Option<String>,
    /// Status after the action.
    pub to_status: Option<String>,
    /// Error description (for failed intents).
    pub error: Option<String>,
    /// ISO-8601 timestamp.
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
        PersistenceEnvelope {
            sequence_nr: 0,
            event_type: event_type.to_string(),
            payload,
            metadata: EventMetadata {
                event_id: uuid::Uuid::new_v4(),
                causation_id: uuid::Uuid::new_v4(),
                correlation_id: uuid::Uuid::new_v4(),
                timestamp: chrono::Utc::now(),
                actor_id: "store-test".to_string(),
            },
        }
    }

    fn sqlite_test_url(test_name: &str) -> String {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "temper-store-turso-{test_name}-{}.db",
            uuid::Uuid::new_v4()
        ));
        format!("file:{}", path.display())
    }

    async fn make_store(test_name: &str) -> TursoEventStore {
        TursoEventStore::new(&sqlite_test_url(test_name), None)
            .await
            .expect("create store")
    }

    #[tokio::test]
    async fn append_and_read_events_roundtrip() {
        let store = make_store("append-read").await;
        let persistence_id = "tenant-a:Order:ord-1";

        let new_seq = store
            .append(
                persistence_id,
                0,
                &[
                    test_envelope("OrderCreated", serde_json::json!({ "id": "ord-1" })),
                    test_envelope("OrderApproved", serde_json::json!({ "approved": true })),
                ],
            )
            .await
            .unwrap();

        assert_eq!(new_seq, 2);

        let events = store.read_events(persistence_id, 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence_nr, 1);
        assert_eq!(events[1].sequence_nr, 2);
        assert_eq!(events[0].event_type, "OrderCreated");
        assert_eq!(events[1].event_type, "OrderApproved");
    }

    #[tokio::test]
    async fn append_with_wrong_sequence_fails_with_concurrency_violation() {
        let store = make_store("concurrency").await;
        let persistence_id = "tenant-a:Order:ord-2";

        store
            .append(
                persistence_id,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "id": "ord-2" }),
                )],
            )
            .await
            .unwrap();

        let err = store
            .append(
                persistence_id,
                0,
                &[test_envelope(
                    "OrderUpdated",
                    serde_json::json!({ "step": 2 }),
                )],
            )
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            PersistenceError::ConcurrencyViolation {
                expected: 0,
                actual: 1
            }
        ));
    }

    #[tokio::test]
    async fn snapshot_save_and_load_roundtrip() {
        let store = make_store("snapshot").await;
        let persistence_id = "tenant-a:Order:ord-3";

        store
            .save_snapshot(persistence_id, 5, b"{\"status\":\"created\"}")
            .await
            .unwrap();

        let snapshot = store.load_snapshot(persistence_id).await.unwrap();
        assert_eq!(snapshot, Some((5, b"{\"status\":\"created\"}".to_vec())));

        store
            .save_snapshot(persistence_id, 8, b"{\"status\":\"shipped\"}")
            .await
            .unwrap();

        let updated = store.load_snapshot(persistence_id).await.unwrap();
        assert_eq!(updated, Some((8, b"{\"status\":\"shipped\"}".to_vec())));
    }

    #[tokio::test]
    async fn list_entity_ids_returns_distinct_pairs() {
        let store = make_store("entity-list").await;

        let tenant_a = format!("tenant-a-{}", uuid::Uuid::new_v4());
        let tenant_b = format!("tenant-b-{}", uuid::Uuid::new_v4());

        let order_1 = format!("{tenant_a}:Order:ord-1");
        let order_2 = format!("{tenant_a}:Order:ord-2");
        let task_1 = format!("{tenant_a}:Task:task-1");
        let other_tenant = format!("{tenant_b}:Order:ord-9");

        store
            .append(
                &order_1,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "id": "ord-1" }),
                )],
            )
            .await
            .unwrap();
        store
            .append(
                &order_1,
                1,
                &[test_envelope(
                    "OrderUpdated",
                    serde_json::json!({ "step": 2 }),
                )],
            )
            .await
            .unwrap();
        store
            .append(
                &order_2,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "id": "ord-2" }),
                )],
            )
            .await
            .unwrap();
        store
            .append(
                &task_1,
                0,
                &[test_envelope(
                    "TaskCreated",
                    serde_json::json!({ "id": "task-1" }),
                )],
            )
            .await
            .unwrap();
        store
            .append(
                &other_tenant,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "id": "ord-9" }),
                )],
            )
            .await
            .unwrap();

        let mut entities = store.list_entity_ids(&tenant_a).await.unwrap();
        entities.sort();

        assert_eq!(
            entities,
            vec![
                ("Order".to_string(), "ord-1".to_string()),
                ("Order".to_string(), "ord-2".to_string()),
                ("Task".to_string(), "task-1".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn migrate_is_idempotent() {
        let store = make_store("migrate-idempotent").await;

        store.migrate().await.unwrap();
        store.migrate().await.unwrap();
    }
}
