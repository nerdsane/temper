use serde::{Deserialize, Serialize};

/// Event type used for the parent-journal record of a Composite action.
///
/// Concrete sub-write events remain the state-changing events on their target
/// journals. This event records the composite intent and the exact sub-write
/// journals/idempotency keys that were committed atomically with it.
pub const COMPOSITE_EVENT_TYPE: &str = "CompositeEvent";

/// Replay/audit record for one Composite action application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeEvent {
    pub tenant: String,
    pub parent_entity_type: String,
    pub parent_entity_id: String,
    pub parent_action: String,
    pub composite_idempotency_key: String,
    pub sub_writes: Vec<CompositeEventSubWrite>,
}

/// One concrete sub-write recorded in a [`CompositeEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeEventSubWrite {
    pub index: usize,
    pub entity_type: String,
    pub entity_id: String,
    pub action: String,
    pub idempotency_key: String,
}

/// Marker trait for domain events.
/// Events must be serializable (for persistence) and Send + 'static (for async).
pub trait DomainEvent:
    Send + Serialize + for<'de> Deserialize<'de> + std::fmt::Debug + 'static
{
}

/// Metadata attached to every persisted event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMetadata {
    /// Unique ID of this event.
    pub event_id: uuid::Uuid,
    /// ID of the command/message that caused this event.
    pub causation_id: uuid::Uuid,
    /// Correlation ID for tracing across actor boundaries.
    pub correlation_id: uuid::Uuid,
    /// Timestamp of persistence.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Actor that produced this event.
    pub actor_id: String,
}

/// Trait for event-sourced persistent actors.
/// Extends the base Actor trait with event journal and snapshot capabilities.
///
/// The persistence protocol:
/// 1. Actor receives command (message)
/// 2. Handler validates command against current state
/// 3. Handler produces events via ctx.persist(event)
/// 4. Events are written to journal (Postgres)
/// 5. Events are applied to state via apply_event()
/// 6. Periodically, state is snapshotted for fast recovery
///
/// On restart:
/// 1. Load latest snapshot (if any)
/// 2. Replay events since snapshot
/// 3. Actor state is rebuilt — ready to process messages
pub trait PersistentActor: Send + 'static {
    type Event: DomainEvent;
    type State: Send + Serialize + for<'de> Deserialize<'de> + 'static;

    /// The persistence ID. Must be unique across the system.
    /// Typically: "{entity_type}:{entity_id}"
    fn persistence_id(&self) -> &str;

    /// Apply a single event to the state. Must be pure (no side effects).
    /// This is called during replay and during live operation.
    fn apply_event(state: &mut Self::State, event: &Self::Event);

    /// How often to snapshot (every N events). Default: every 100 events.
    fn snapshot_every(&self) -> u64 {
        100
    }
}

/// Trait for the event store backend (implemented by temper-store-postgres).
/// Uses desugared async-in-trait to enforce Send bounds on futures.
pub trait EventStore: Send + Sync + 'static {
    /// Append events to the journal.
    fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> impl std::future::Future<Output = Result<u64, PersistenceError>> + Send;

    /// Atomically append events to multiple journals.
    ///
    /// Backends must either commit every append in `appends`, or commit none.
    /// This is the storage primitive composite actions need before they can
    /// persist cross-actor sub-writes as one physical unit.
    fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> impl std::future::Future<Output = Result<Vec<PersistenceAppendResult>, PersistenceError>> + Send;

    /// Read events from the journal, starting after the given sequence number.
    fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> impl std::future::Future<Output = Result<Vec<PersistenceEnvelope>, PersistenceError>> + Send;

    /// Save a state snapshot.
    fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> impl std::future::Future<Output = Result<(), PersistenceError>> + Send;

    /// Load the latest snapshot.
    fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<(u64, Vec<u8>)>, PersistenceError>> + Send;

    /// List all distinct `(entity_type, entity_id)` pairs for a tenant.
    fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> impl std::future::Future<Output = Result<Vec<(String, String)>, PersistenceError>> + Send;

    /// List distinct entity IDs for one `(tenant, entity_type)` pair.
    fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, PersistenceError>> + Send;

    /// List at most `limit` authoritative `(entity_type, entity_id)` pairs for
    /// a tenant, optionally scoped to one entity type.
    ///
    /// Storage backends should override this to apply the bound inside the
    /// backing query. The default is intended for small in-memory/test stores.
    fn list_entity_ids_limited(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<(String, String)>, PersistenceError>> + Send
    {
        async move {
            if limit == 0 {
                return Ok(Vec::new());
            }
            let mut entities = if let Some(entity_type) = entity_type {
                self.list_entity_ids_by_type(tenant, entity_type)
                    .await?
                    .into_iter()
                    .map(|entity_id| (entity_type.to_string(), entity_id))
                    .collect::<Vec<_>>()
            } else {
                self.list_entity_ids(tenant).await?
            };
            entities.sort();
            entities.truncate(limit);
            Ok(entities)
        }
    }
}

/// A persisted event with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceEnvelope {
    /// Monotonic sequence number within the entity's journal.
    pub sequence_nr: u64,
    /// Fully qualified event type name.
    pub event_type: String,
    /// Serialized event payload.
    pub payload: serde_json::Value,
    /// Event metadata (causation, correlation, timestamp).
    pub metadata: EventMetadata,
}

/// One stream append inside an atomic multi-journal append.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceAppend {
    /// Persistence ID in the form `{tenant}:{entity_type}:{entity_id}`.
    pub persistence_id: String,
    /// Optimistic-concurrency sequence expected before this append.
    pub expected_sequence: u64,
    /// Events to append to this journal.
    pub events: Vec<PersistenceEnvelope>,
}

/// New sequence number for one stream after an atomic batch append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceAppendResult {
    /// Persistence ID that was appended.
    pub persistence_id: String,
    /// New highest sequence number for this journal.
    pub sequence_nr: u64,
}

/// Errors that can occur during event persistence operations.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    /// Optimistic concurrency check failed (another writer appended first).
    #[error("optimistic concurrency violation: expected sequence {expected}, got {actual}")]
    ConcurrencyViolation { expected: u64, actual: u64 },

    /// Event serialization or deserialization failed.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Underlying storage backend returned an error.
    #[error("storage error: {0}")]
    Storage(String),
}

/// Convert backend-specific errors into [`PersistenceError::Storage`].
pub fn storage_error(err: impl std::fmt::Display) -> PersistenceError {
    PersistenceError::Storage(err.to_string())
}
