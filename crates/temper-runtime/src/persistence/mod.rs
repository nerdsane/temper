use serde::{Deserialize, Serialize};

mod batch;
mod event_store_index;
mod event_store_journal;
mod index;
mod materialization;
mod types;
pub use batch::{PersistenceAppend, PersistenceAppendResult, PersistenceBatchIdempotency};
pub use index::{
    EntityKeyLookup, EntityKeyRow, EntityVectorCandidate, EntityVectorRow, IndexReconciliation,
    KeyContractActivation, KeyIndexBackfillFence, ProjectionSourceFence, SnapshotBackfillFence,
    SnapshotSourceFence, decode_activated_key_contract, encode_activated_key_contract, pack_f32_le,
    unpack_f32_le,
};
pub use materialization::{
    STATE_MATERIALIZATION_EVENT_TYPE, STATE_MATERIALIZATION_IDEMPOTENCY_KEY_BUDGET,
    STATE_MATERIALIZATION_PAYLOAD_BYTE_BUDGET, STATE_MATERIALIZATION_SCHEMA,
    is_state_materialization_event_for, is_state_materialization_payload_for,
    serialized_json_fits_byte_budget,
};
pub use types::{
    JournalBoundary, KeyReconciliationEntity, PersistenceEnvelope, PersistenceError, storage_error,
};

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
    /// Versioned digest of the complete normalized composite intent.
    #[serde(default)]
    pub intent_hash: String,
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
    event_store_index::event_store_index_methods!();
    event_store_journal::event_store_journal_methods!();
}
