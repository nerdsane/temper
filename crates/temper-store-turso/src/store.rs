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

    /// Run schema migrations on connect.
    async fn migrate(&self) -> Result<(), PersistenceError> {
        let conn = self.connection()?;

        conn.execute(schema::CREATE_EVENTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVENTS_ENTITY_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_SNAPSHOTS_TABLE, ())
            .await
            .map_err(storage_error)?;

        Ok(())
    }

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
        let conn = self.connection()?;
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
        let conn = self.connection()?;

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
        let conn = self.connection()?;

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
        let conn = self.connection()?;
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
        let conn = self.connection()?;
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
