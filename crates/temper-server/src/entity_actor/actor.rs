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
use std::time::Instant;

use temper_jit::table::{Effect, TransitionTable};
use temper_observe::wide_event;
use temper_runtime::actor::{Actor, ActorContext, ActorError};
use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, EventMetadata, JournalRead, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};

use crate::storage::{BackendLabel, BoxedEventStore};

use super::effects::{
    FieldSyncMode, build_eval_context_with_xref, process_action_with_xref_and_field_mode,
    prune_transient_action_fields_from_state,
};
use super::event_persistence::{
    PersistedStateTimeoutClock, STATE_TIMEOUT_CLOCK_SNAPSHOT_AUTHORITY_KEY,
    apply_legacy_state_timeout_clock, apply_replayed_state_timeout_clock,
    apply_state_timeout_clock, decode_entity_event_clock, encode_entity_event_payload,
    entity_event_type, is_entity_tombstone, state_timeout_clock_after_event,
};
use super::snapshot_queue::{SnapshotEnqueueOutcome, SnapshotWriteQueue};

mod action;
mod contract;
mod persistence;
mod replay;
mod startup;
use super::types::{
    EntityEvent, EntityMsg, EntityResponse, EntityState, MAX_EVENTS_SINCE_SNAPSHOT,
    MAX_ITEMS_PER_ENTITY, STATE_TIMEOUT_PRECONDITION_MISMATCH, StateTimeoutPrecondition,
};
pub(crate) use replay::recover_entity_state_from_store;

struct EntityActionRequest {
    name: String,
    params: serde_json::Value,
    cross_entity_booleans: BTreeMap<String, bool>,
    idempotency_key: Option<String>,
    state_timeout_precondition: Option<Box<StateTimeoutPrecondition>>,
}

fn state_timeout_precondition_is_stale(
    table: &TransitionTable,
    state: &EntityState,
    precondition: Option<&StateTimeoutPrecondition>,
) -> bool {
    precondition.is_some_and(|precondition| {
        !table
            .state_timeouts
            .iter()
            .any(|timeout| timeout == &precondition.expected_timeout)
            || state.status != precondition.expected_state
            || state.state_timeout_clock_reset_at != precondition.expected_reset_at
            || state.state_timeout_clock_reset_version != precondition.expected_reset_version
    })
}

fn event_budget_workspace_id(state: &EntityState) -> String {
    if state.entity_type == "Workspace" {
        return state.entity_id.clone();
    }

    for key in ["WorkspaceId", "workspace_id"] {
        if let Some(value) = state
            .fields
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
        {
            return value.to_string();
        }
    }

    String::new()
}

fn duplicate_idempotency_custom_effects(
    table: &TransitionTable,
    state: &EntityState,
    action: &str,
    cross_entity_booleans: &BTreeMap<String, bool>,
) -> Vec<String> {
    if !table.composite_actions.contains_key(action) {
        return Vec::new();
    }

    let ctx = build_eval_context_with_xref(state, cross_entity_booleans);
    table
        .evaluate_ctx(&state.status, &ctx, action)
        .filter(|result| result.success)
        .map(|result| {
            result
                .effects
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::Custom(name) => Some(name),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

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
        if let Some(obj) = fields.as_object_mut() {
            obj.entry("Id".to_string())
                .or_insert(serde_json::Value::String(entity_id.to_string()));
            obj.entry("Status".to_string())
                .or_insert(serde_json::Value::String(table.initial_state.clone()));
        }

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
            state_timeout_clock_reset_at: None,
            state_timeout_clock_reset_version: None,
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: BTreeMap::new(),
        }
    }

    /// Advance timeout clock metadata with the table that committed the event.
    ///
    /// Journaled events persist this exact outcome atomically with the event;
    /// in-memory entities use the same calculation directly. Unrelated
    /// self-loops retain the prior anchor, and leaving a timed state clears it.
    pub(super) fn update_state_timeout_clock(
        table: &TransitionTable,
        state: &mut EntityState,
        event: &EntityEvent,
    ) {
        let next_total_event = u64::try_from(state.total_event_count)
            .unwrap_or(u64::MAX)
            .checked_add(1)
            .expect("state timeout clock version overflow");
        let event_version = if state.sequence_nr != 0 {
            state.sequence_nr
        } else {
            next_total_event
        };
        let clock = state_timeout_clock_after_event(table, state, event, event_version);
        apply_state_timeout_clock(state, clock);
    }

    fn validate_journal_read(
        persistence_id: &str,
        from_sequence: u64,
        read: &JournalRead,
    ) -> Result<(), ActorError> {
        if read.journal_head_sequence_nr < from_sequence {
            return Err(ActorError::custom(format!(
                "journal head {} precedes replay boundary {from_sequence} for {persistence_id}",
                read.journal_head_sequence_nr
            )));
        }

        let mut observed_sequence = from_sequence;
        for event in &read.events {
            let expected_sequence = observed_sequence.checked_add(1).ok_or_else(|| {
                ActorError::custom(format!(
                    "journal sequence overflow after {observed_sequence} for {persistence_id}"
                ))
            })?;
            if event.sequence_nr != expected_sequence {
                return Err(ActorError::custom(format!(
                    "journal replay gap for {persistence_id}: expected {expected_sequence}, got {}",
                    event.sequence_nr
                )));
            }
            observed_sequence = event.sequence_nr;
        }

        if observed_sequence != read.journal_head_sequence_nr {
            return Err(ActorError::custom(format!(
                "incomplete journal replay for {persistence_id}: stopped at {observed_sequence}, captured head is {}",
                read.journal_head_sequence_nr
            )));
        }
        Ok(())
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
    pub(crate) fn serialize_snapshot_state(
        state: &EntityState,
    ) -> Result<Vec<u8>, PersistenceError> {
        let mut value = serde_json::to_value(state)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        if let Some(obj) = value.as_object_mut() {
            obj.remove("events");
            if let Some(reset_at) = state.state_timeout_clock_reset_at {
                obj.insert(
                    "state_timeout_clock_reset_at".to_string(),
                    serde_json::json!(reset_at),
                );
            }
            if let Some(reset_version) = state.state_timeout_clock_reset_version {
                obj.insert(
                    "state_timeout_clock_reset_version".to_string(),
                    serde_json::json!(reset_version),
                );
            }
            obj.insert("events_since_snapshot".to_string(), serde_json::json!(0));
            obj.insert(
                "last_snapshot_sequence_nr".to_string(),
                serde_json::json!(state.sequence_nr),
            );
            obj.insert(
                STATE_TIMEOUT_CLOCK_SNAPSHOT_AUTHORITY_KEY.to_string(),
                serde_json::Value::Bool(true),
            );
        }
        serde_json::to_vec(&value).map_err(|e| PersistenceError::Serialization(e.to_string()))
    }

    /// Attempt to load actor state from snapshot payload bytes.
    fn apply_snapshot_bytes(
        state: &mut EntityState,
        sequence_nr: u64,
        bytes: &[u8],
    ) -> Option<bool> {
        let mut value = match serde_json::from_slice::<serde_json::Value>(bytes) {
            Ok(v) => v,
            Err(_) => return None,
        };
        let obj = value.as_object_mut()?;

        let has_explicit_clock_authority =
            match obj.remove(STATE_TIMEOUT_CLOCK_SNAPSHOT_AUTHORITY_KEY) {
                Some(serde_json::Value::Bool(true)) => true,
                Some(_) => return None,
                None => false,
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
                let complete_legacy_pair = match (
                    restored.state_timeout_clock_reset_at,
                    restored.state_timeout_clock_reset_version,
                ) {
                    (Some(_), Some(version)) if version > 0 => true,
                    (Some(_), Some(_)) => return None,
                    _ => false,
                };
                let clock_authoritative = has_explicit_clock_authority || complete_legacy_pair;
                let valid_authoritative_pair = match (
                    restored.state_timeout_clock_reset_at,
                    restored.state_timeout_clock_reset_version,
                ) {
                    (None, None) => true,
                    (Some(_), Some(version)) => version > 0,
                    _ => false,
                };
                if clock_authoritative && !valid_authoritative_pair {
                    return None;
                }
                restored.sequence_nr = sequence_nr;
                restored.events_since_snapshot = 0;
                restored.last_snapshot_sequence_nr = sequence_nr;
                *state = restored;
                Some(clock_authoritative)
            }
            Err(_) => None,
        }
    }

    fn validate_snapshot_timeout_clock_against_journal_head(
        persistence_id: &str,
        state: &EntityState,
        journal_head_sequence_nr: u64,
        clock_authoritative: bool,
    ) -> Result<(), ActorError> {
        if !clock_authoritative {
            return Ok(());
        }

        match (
            state.state_timeout_clock_reset_at,
            state.state_timeout_clock_reset_version,
        ) {
            (None, None) => Ok(()),
            (Some(_), Some(reset_version))
                if reset_version > 0 && reset_version <= journal_head_sequence_nr =>
            {
                Ok(())
            }
            (reset_at, reset_version) => Err(ActorError::custom(format!(
                "invalid authoritative state-timeout clock in snapshot for {persistence_id}: \
                 reset_at={reset_at:?}, reset_version={reset_version:?}, \
                 journal_head_sequence_nr={journal_head_sequence_nr}"
            ))),
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

#[cfg(test)]
#[path = "actor_test.rs"]
mod tests;

#[cfg(test)]
#[path = "actor_timeout_clock_tests.rs"]
mod timeout_clock_tests;

#[cfg(all(test, feature = "sim"))]
#[path = "actor_timeout_clock_migration_tests.rs"]
mod timeout_clock_migration_tests;

#[cfg(all(test, feature = "sim"))]
#[path = "actor_tombstone_replay_tests.rs"]
mod tombstone_replay_tests;
