//! PostgreSQL-backed implementation of the [`EventStore`] trait.
//!
//! The store uses a `sqlx::PgPool` for all database access and relies on the
//! `UNIQUE (entity_type, entity_id, sequence_nr)` constraint to enforce
//! optimistic concurrency on appends.

use std::time::Instant;

use sqlx::{Acquire, PgPool};
use temper_runtime::persistence::{
    EntityVectorCandidate, EntityVectorRow, EventMetadata, EventStore, JournalRead,
    PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError, pack_f32_le,
    unpack_f32_le,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use crate::metrics::{
    PostgresTransactionTimer, record_postgres_pool_acquire_duration,
    record_postgres_transaction_begin_duration, record_postgres_transaction_commit_duration,
};
use crate::segments;

mod append;
mod batch_reads;
mod contract;
mod indexes;
mod snapshots;

const EVENT_APPEND_OPERATION: &str = "event_append";

/// A PostgreSQL-backed event store.
///
/// Persistence IDs follow `"tenant:entity_type:entity_id"` (with legacy
/// `"entity_type:entity_id"` mapped to tenant `"default"`). Components are
/// stored in separate columns for efficient filtering.
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
// EventStore implementation
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "store_projection_test.rs"]
mod projection_tests;

#[cfg(test)]
#[path = "store/tests/mod.rs"]
mod tests;
