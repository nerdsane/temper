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

/// Split a `"entity_type:entity_id"` persistence ID into its two parts.
///
/// Returns `Err(PersistenceError::Storage)` when the format is invalid.
pub fn parse_persistence_id(persistence_id: &str) -> Result<(&str, &str), PersistenceError> {
    let colon_pos = persistence_id.find(':').ok_or_else(|| {
        PersistenceError::Storage(format!(
            "invalid persistence_id (expected 'entity_type:entity_id'): {persistence_id}"
        ))
    })?;
    let entity_type = &persistence_id[..colon_pos];
    let entity_id = &persistence_id[colon_pos + 1..];
    if entity_type.is_empty() || entity_id.is_empty() {
        return Err(PersistenceError::Storage(format!(
            "invalid persistence_id (empty entity_type or entity_id): {persistence_id}"
        )));
    }
    Ok((entity_type, entity_id))
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
        let (entity_type, entity_id) = parse_persistence_id(persistence_id)?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        // Verify the current maximum sequence_nr matches what the caller
        // expects.  This is the "read" side of optimistic locking.
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT COALESCE(MAX(sequence_nr), 0) FROM events \
             WHERE entity_type = $1 AND entity_id = $2",
        )
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
                "INSERT INTO events (entity_type, entity_id, sequence_nr, event_type, payload, metadata) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(entity_type)
            .bind(entity_id)
            .bind(new_seq as i64)
            .bind(&event.event_type)
            .bind(&event.payload)
            .bind(metadata_json)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                // If the database reports a unique-violation we surface it
                // as a concurrency error.  sqlx wraps PG errors in
                // `sqlx::Error::Database`.
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
        let (entity_type, entity_id) = parse_persistence_id(persistence_id)?;

        let rows: Vec<(i64, String, serde_json::Value, serde_json::Value)> = sqlx::query_as(
            "SELECT sequence_nr, event_type, payload, metadata \
             FROM events \
             WHERE entity_type = $1 AND entity_id = $2 AND sequence_nr > $3 \
             ORDER BY sequence_nr ASC",
        )
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
        let (entity_type, entity_id) = parse_persistence_id(persistence_id)?;

        sqlx::query(
            "INSERT INTO snapshots (entity_type, entity_id, sequence_nr, state) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (entity_type, entity_id) \
             DO UPDATE SET sequence_nr = $3, state = $4, created_at = now()",
        )
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
        let (entity_type, entity_id) = parse_persistence_id(persistence_id)?;

        let row: Option<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT sequence_nr, state FROM snapshots \
             WHERE entity_type = $1 AND entity_id = $2",
        )
        .bind(entity_type)
        .bind(entity_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| PersistenceError::Storage(e.to_string()))?;

        Ok(row.map(|(seq, state)| (seq as u64, state)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- persistence_id parsing ---------------------------------------------

    #[test]
    fn parse_valid_persistence_id() {
        let (entity_type, entity_id) = parse_persistence_id("Order:abc-123").unwrap();
        assert_eq!(entity_type, "Order");
        assert_eq!(entity_id, "abc-123");
    }

    #[test]
    fn parse_persistence_id_with_multiple_colons() {
        // Only the first colon is the delimiter; the rest belong to the id.
        let (entity_type, entity_id) = parse_persistence_id("Order:abc:def:ghi").unwrap();
        assert_eq!(entity_type, "Order");
        assert_eq!(entity_id, "abc:def:ghi");
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
    fn parse_persistence_id_empty_entity_type() {
        let err = parse_persistence_id(":abc-123").unwrap_err();
        assert!(
            matches!(err, PersistenceError::Storage(_)),
            "expected Storage error for empty entity_type, got: {err:?}"
        );
    }

    #[test]
    fn parse_persistence_id_empty_entity_id() {
        let err = parse_persistence_id("Order:").unwrap_err();
        assert!(
            matches!(err, PersistenceError::Storage(_)),
            "expected Storage error for empty entity_id, got: {err:?}"
        );
    }
}
