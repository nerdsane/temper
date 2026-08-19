//! EntityActor: processes actions through a TransitionTable.
//!
//! This is the bridge between the actor runtime and the I/O Automaton specs.
//! Each entity actor holds its current state and a TransitionTable, and
//! processes action messages by evaluating transitions through the table.
//!
//! The same TransitionTable used here is also used by:
//! - Stateright model checking (Level 1)
//! - Deterministic simulation (Level 2)
//! - Property-based tests (Level 3)
//!
//! So if it passes verification, it works correctly here.
//!
//! ## TigerStyle Principles Applied
//!
//! - **Assertions in production**: Pre/postcondition assertions on every transition.
//!   Status must be in the valid state set. Item count must not go negative.
//!   Event log must grow monotonically. These are not debug-only -- they run always.
//! - **Bounded execution**: Max events per entity (10,000), max items (1,000).
//!   No unbounded growth. Violations are detected immediately, not at OOM.
//! - **Explicit error handling**: Every match arm handled. No unwrap on user input.
//! - **Deterministic**: Same input -> same output. No randomness in transition logic.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

use temper_jit::table::TransitionTable;
use temper_runtime::persistence::PersistenceError;
use temper_runtime::scheduler::sim_uuid;

use crate::storage::{BackendLabel, BoxedEventStore};

use super::snapshot_queue::SnapshotWriteQueue;
use super::types::EntityState;
/// The entity actor -- processes actions through a TransitionTable.
/// Optionally persists events to the configured backend. Wide events are emitted
/// via the OTEL SDK (no-op when OTEL is not initialised).
pub struct EntityActor {
    tenant: String,
    entity_type: String,
    entity_id: String,
    /// Live reference to the transition table. Reads through `RwLock` so that
    /// hot-swapped tables are visible on the next action dispatch without
    /// restarting the actor.
    table: Arc<RwLock<TransitionTable>>,
    initial_fields: serde_json::Value,
    /// Optional event journal for persistence. None = in-memory only.
    event_journal: Option<BoxedEventStore>,
    /// Optional async snapshot writer. Event appends remain synchronous.
    snapshot_queue: Option<Arc<SnapshotWriteQueue>>,
    /// Persistence backend label used for metrics and backend-specific field sync.
    event_backend: Option<BackendLabel>,
    /// Trace ID for correlating all events from this actor.
    trace_id: String,
    /// Shared idempotency cache (ADR-0048 sub-decision 5). Consulted before
    /// executing an action whose `idempotency_key` is set, so dispatch-layer
    /// retries that race past the caller's timeout cannot double-execute.
    idempotency_cache: Option<Arc<crate::idempotency::IdempotencyCache>>,
    /// Object store for field-overflow blob bytes. SQL stores only refs.
    blob_store: Option<crate::blob_store::BlobStore>,
}

impl EntityActor {
    fn build_initial_state(
        entity_type: &str,
        entity_id: &str,
        table: &TransitionTable,
        initial_fields: &serde_json::Value,
    ) -> EntityState {
        let mut fields = initial_fields.clone();
        super::effects::canonicalize_entity_fields(&mut fields, entity_id, &table.initial_state);

        EntityState {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            status: table.initial_state.clone(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields,
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: BTreeMap::new(),
        }
    }

    /// Snapshot frequency in events.
    ///
    /// Controlled by `TEMPER_SNAPSHOT_INTERVAL` (default 100).
    fn snapshot_interval() -> u64 {
        static SNAPSHOT_INTERVAL: OnceLock<u64> = OnceLock::new();
        *SNAPSHOT_INTERVAL.get_or_init(|| {
            std::env::var("TEMPER_SNAPSHOT_INTERVAL") // determinism-ok: read once at startup
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(100)
        })
    }

    /// Serialize actor state for snapshot persistence, excluding recent event history.
    ///
    /// The stored snapshot is already a segment boundary, so its hot tail budget
    /// is reset in the payload. Lifetime sequence/count fields remain intact.
    fn serialize_snapshot_state(state: &EntityState) -> Result<Vec<u8>, PersistenceError> {
        let mut value = serde_json::to_value(state)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        if let Some(obj) = value.as_object_mut() {
            obj.remove("events");
            obj.insert("events_since_snapshot".to_string(), serde_json::json!(0));
            obj.insert(
                "last_snapshot_sequence_nr".to_string(),
                serde_json::json!(state.sequence_nr),
            );
        }
        serde_json::to_vec(&value).map_err(|e| PersistenceError::Serialization(e.to_string()))
    }

    /// Attempt to load actor state from snapshot payload bytes.
    fn apply_snapshot_bytes(state: &mut EntityState, sequence_nr: u64, bytes: &[u8]) -> bool {
        let mut value = match serde_json::from_slice::<serde_json::Value>(bytes) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let Some(obj) = value.as_object_mut() else {
            return false;
        };

        // Snapshot intentionally excludes in-memory recent history.
        obj.insert("events".to_string(), serde_json::json!([]));
        if !obj.contains_key("total_event_count") {
            obj.insert(
                "total_event_count".to_string(),
                serde_json::json!(sequence_nr as usize),
            );
        }
        obj.insert("events_since_snapshot".to_string(), serde_json::json!(0));
        obj.insert(
            "last_snapshot_sequence_nr".to_string(),
            serde_json::json!(sequence_nr),
        );

        match serde_json::from_value::<EntityState>(value) {
            Ok(mut restored) => {
                if restored.entity_type != state.entity_type
                    || restored.entity_id != state.entity_id
                {
                    return false;
                }
                super::effects::canonicalize_entity_fields(
                    &mut restored.fields,
                    &state.entity_id,
                    &restored.status,
                );
                restored.sequence_nr = sequence_nr;
                restored.events_since_snapshot = 0;
                restored.last_snapshot_sequence_nr = sequence_nr;
                *state = restored;
                true
            }
            Err(_) => false,
        }
    }

    /// Create a new entity actor (in-memory only, no persistence).
    pub fn new(
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
        table: Arc<RwLock<TransitionTable>>,
        initial_fields: serde_json::Value,
    ) -> Self {
        Self {
            tenant: "default".into(),
            entity_type: entity_type.into(),
            entity_id: entity_id.into(),
            table,
            initial_fields,
            event_journal: None,
            snapshot_queue: None,
            event_backend: None,
            trace_id: sim_uuid().to_string(),
            idempotency_cache: None,
            blob_store: None,
        }
    }

    /// Create a new entity actor with persistence.
    pub fn with_persistence(
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
        table: Arc<RwLock<TransitionTable>>,
        initial_fields: serde_json::Value,
        store: BoxedEventStore,
        backend: BackendLabel,
    ) -> Self {
        Self {
            tenant: "default".into(),
            entity_type: entity_type.into(),
            entity_id: entity_id.into(),
            table,
            initial_fields,
            event_journal: Some(store),
            snapshot_queue: None,
            event_backend: Some(backend),
            trace_id: sim_uuid().to_string(),
            idempotency_cache: None,
            blob_store: None,
        }
    }

    /// Set the tenant for this actor (must be called before spawning).
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// Attach the background snapshot writer for this actor's event journal.
    pub(crate) fn with_snapshot_queue(mut self, queue: Option<Arc<SnapshotWriteQueue>>) -> Self {
        self.snapshot_queue = queue;
        self
    }

    /// Attach a shared idempotency cache for actor-side dedup
    /// (ADR-0048 sub-decision 5).
    pub fn with_idempotency_cache(
        mut self,
        cache: Arc<crate::idempotency::IdempotencyCache>,
    ) -> Self {
        self.idempotency_cache = Some(cache);
        self
    }

    /// Attach the object store used for field-overflow blob writes.
    pub(crate) fn with_blob_store(
        mut self,
        blob_store: Option<crate::blob_store::BlobStore>,
    ) -> Self {
        self.blob_store = blob_store;
        self
    }
}

mod handle;
mod persist;

pub(crate) use persist::{
    recover_authoritative_entity_state_from_store, recover_entity_state_from_store,
};

#[cfg(test)]
pub(crate) use handle::event_budget_workspace_id;

#[cfg(test)]
#[path = "../actor_test.rs"]
mod tests;

#[cfg(test)]
#[path = "../authoritative_replay_test.rs"]
mod authoritative_replay_tests;
