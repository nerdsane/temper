//! PostgreSQL-backed implementation of the [`EventStore`] trait.
//!
//! The store uses a `sqlx::PgPool` for all database access and relies on the
//! `UNIQUE (entity_type, entity_id, sequence_nr)` constraint to enforce
//! optimistic concurrency on appends.

use sqlx::PgPool;
use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceEnvelope, PersistenceError,
};

/// A PostgreSQL-backed event store.
///
/// Persistence IDs follow the `"entity_type:entity_id"` convention. Both
/// halves are stored in separate columns so that queries can efficiently
/// target all events for a given entity type.
#[derive(Clone, Debug)]
pub struct PostgresEventStore {
    pool: PgPool,
}

impl PostgresEventStore {
    /// Create a new store backed by the given connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Return a reference to the inner pool (useful for migrations).
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Split a persistence ID into `(tenant, entity_type, entity_id)`.
///
/// Accepts both the new 3-segment format (`tenant:type:id`) and the
/// legacy 2-segment format (`type:id`). Legacy IDs are assigned to
/// the `"default"` tenant.
pub fn parse_persistence_id(persistence_id: &str) -> Result<(&str, &str, &str), PersistenceError> {
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

// ---------------------------------------------------------------------------
// EventStore implementation
// ---------------------------------------------------------------------------

impl EventStore for PostgresEventStore {
    /// Append one or more events to the journal.
    ///
    /// Events are inserted with consecutive sequence numbers starting from
    /// `expected_sequence + 1`.  The UNIQUE index on
    /// `(entity_type, entity_id, sequence_nr)` enforces optimistic
    /// concurrency; a duplicate-key violation is surfaced as
    /// [`PersistenceError::ConcurrencyViolation`].
    ///
    /// Returns the new highest sequence number after the append.
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        let current_seq = row.map(|r| r.0 as u64).unwrap_or(0);
        if current_seq != expected_sequence {
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: current_seq,
            });
        }

        let mut new_seq = expected_sequence;
        for event in events {
            new_seq += 1;
            let metadata_json = serde_json::to_value(&event.metadata)
                .map_err(|e| PersistenceError::Serialization(e.to_string()))?;

            sqlx::query(
                "INSERT INTO events (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(tenant)
            .bind(entity_type)
            .bind(entity_id)
            .bind(new_seq as i64)
            .bind(&event.event_type)
            .bind(&event.payload)
            .bind(metadata_json)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("unique") || msg.contains("duplicate key") {
                    PersistenceError::ConcurrencyViolation {
                        expected: expected_sequence,
                        actual: new_seq,
                    }
                } else {
                    PersistenceError::Storage(msg)
                }
            })?;
        }

        tx.commit()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(new_seq)
    }

    /// Read events from the journal starting after `from_sequence`.
    ///
    /// Events are returned in ascending `sequence_nr` order.
    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;

        let rows: Vec<(i64, String, serde_json::Value, serde_json::Value)> = sqlx::query_as(
            "SELECT sequence_nr, event_type, payload, metadata \
             FROM events \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr > $4 \
             ORDER BY sequence_nr ASC",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(from_sequence as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        rows.into_iter()
            .map(|(seq, event_type, payload, meta_json)| {
                let metadata: EventMetadata = serde_json::from_value(meta_json)
                    .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
                Ok(PersistenceEnvelope {
                    sequence_nr: seq as u64,
                    event_type,
                    payload,
                    metadata,
                })
            })
            .collect()
    }

    /// Save (upsert) a snapshot for the given entity.
    ///
    /// Uses `ON CONFLICT … DO UPDATE` so that only the latest snapshot is
    /// retained per entity.
    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;

        sqlx::query(
            "INSERT INTO snapshots (tenant, entity_type, entity_id, sequence_nr, state) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant, entity_type, entity_id) \
             DO UPDATE SET sequence_nr = $4, state = $5, created_at = now()",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .bind(sequence_nr as i64)
        .bind(snapshot)
        .execute(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Load the latest snapshot for an entity.
    ///
    /// Returns `None` when no snapshot has been saved yet.
    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, entity_type, entity_id) = parse_persistence_id(persistence_id)?;

        let row: Option<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT sequence_nr, state FROM snapshots \
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
        )
        .bind(tenant)
        .bind(entity_type)
        .bind(entity_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(row.map(|(seq, state)| (seq as u64, state)))
    }

    /// List all distinct entities that have at least one persisted event
    /// in the given tenant.
    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT entity_type, entity_id \
             FROM events \
             WHERE tenant = $1",
        )
        .bind(tenant)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::run_migrations;

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

    // -- persistence_id parsing ---------------------------------------------

    #[test]
    fn parse_3_segment_persistence_id() {
        let (tenant, entity_type, entity_id) = parse_persistence_id("alpha:Order:abc-123").unwrap();
        assert_eq!(tenant, "alpha");
        assert_eq!(entity_type, "Order");
        assert_eq!(entity_id, "abc-123");
    }

    #[test]
    fn parse_legacy_2_segment_persistence_id() {
        let (tenant, entity_type, entity_id) = parse_persistence_id("Order:abc-123").unwrap();
        assert_eq!(tenant, "default");
        assert_eq!(entity_type, "Order");
        assert_eq!(entity_id, "abc-123");
    }

    #[test]
    fn parse_3_segment_with_colons_in_id() {
        // splitn(3, ':') puts everything after the second colon into entity_id
        let (tenant, entity_type, entity_id) = parse_persistence_id("beta:Task:T-1:sub").unwrap();
        assert_eq!(tenant, "beta");
        assert_eq!(entity_type, "Task");
        assert_eq!(entity_id, "T-1:sub");
    }

    #[test]
    fn parse_persistence_id_missing_colon() {
        let err = parse_persistence_id("OrderAbc123").unwrap_err();
        assert!(
            matches!(err, PersistenceError::Storage(_)),
            "expected Storage error, got: {err:?}"
        );
    }

    #[test]
    fn parse_persistence_id_empty_segment() {
        assert!(parse_persistence_id(":Order:abc").is_err());
        assert!(parse_persistence_id("tenant::abc").is_err());
        assert!(parse_persistence_id("tenant:Order:").is_err());
        assert!(parse_persistence_id(":abc").is_err());
        assert!(parse_persistence_id("Order:").is_err());
    }

    #[test]
    fn list_entity_ids_returns_distinct_pairs() {
        let database_url = match std::env::var("DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!("skipping Postgres integration test: DATABASE_URL is not set");
                return;
            }
        };

        sqlx::test_block_on(async {
            let pool = PgPool::connect(&database_url).await.unwrap();
            run_migrations(&pool).await.unwrap();
            let store = PostgresEventStore::new(pool);

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
                        serde_json::json!({"id":"ord-1"}),
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
                        serde_json::json!({"step": 2}),
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
                        serde_json::json!({"id":"ord-2"}),
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
                        serde_json::json!({"id":"task-1"}),
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
                        serde_json::json!({"id":"ord-9"}),
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
        });
    }
}
