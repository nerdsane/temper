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
//! - **Internal consistency**: Debug assertions check transition pre/postconditions.
//!   User-reachable rejection paths return errors rather than stopping the actor.
//! - **Bounded execution**: Unsnapshotted events are budgeted and recent history
//!   is bounded. Domain counter constraints belong to the specification.
//! - **Explicit error handling**: Every match arm handled. No unwrap on user input.
//! - **Deterministic**: Same input -> same output. No randomness in transition logic.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use temper_jit::table::{Effect, Refusal, TransitionTable};
use temper_observe::wide_event;
use temper_runtime::actor::{Actor, ActorContext, ActorError};
use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, CommitRows, EventMetadata, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
pub(super) use tokio::time::sleep as sleep_persistence_retry; // determinism-ok: production persistence retry backoff

use crate::request_context::AgentContext;
use crate::storage::{BackendLabel, BoxedEventStore};

use super::action_input::process_action_with_blob_prestate;
use super::effects::{
    FieldSyncMode, build_eval_context_with_xref, prune_transient_action_fields_from_state,
};
use super::entity_rows::{EntityRows, EntityRowsHook, RowsInput};
use super::field_updates::NotUpdated;
use super::scheduled_rows::ScheduledRows;
use super::snapshot_queue::{SnapshotEnqueueOutcome, SnapshotWriteQueue};
use super::types::{
    EntityEvent, EntityMsg, EntityResponse, EntityState, MAX_EVENTS_SINCE_SNAPSHOT,
};
use super::watchdog::Doing;

/// Routine walk recovery is quieter than hydration of a live actor.
#[derive(Clone, Copy)]
pub(super) enum ReplayLogging {
    Actor,
    Walk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReplayPolicy {
    StrictSnapshot,
    StrictFullJournal,
}

impl ReplayPolicy {
    fn loads_snapshot(self) -> bool {
        self != Self::StrictFullJournal
    }

    fn strict_event_validation(self) -> bool {
        self == Self::StrictFullJournal
    }
}

/// What the caller of a refused write is told. An oversized change and a full
/// disk are told in their own words (what to do about it); anything else
/// failed to persist.
/// Why a delete decided on a state its record has left was not written.
const STALE_DELETE: &str = "delete authorization became stale; retry against current state";

pub(super) fn persistence_failure(error: &PersistenceError) -> String {
    match error {
        PersistenceError::EventTooLarge { .. } | PersistenceError::StorageFull { .. } => {
            error.to_string()
        }
        _ => format!("persistence failed: {error}"),
    }
}

/// The answer to an action whose event the store did not take. Only an event
/// too large to write is a refusal. `persist_event` has already read the
/// journal after any other failure: a write that landed is not answered
/// here, a concurrency violation or a `not written` error did not land, and
/// only an `outcome unknown` error may have. None of them is a refusal: the
/// action itself was allowed, and a caller may try it again. A store that
/// failed or a disk under the floor makes the store unavailable (503).
fn persistence_failure_reply(state: EntityState, error: &PersistenceError) -> EntityResponse {
    let said = persistence_failure(error);
    match error {
        PersistenceError::EventTooLarge { .. } => EntityResponse::refused(state, said),
        PersistenceError::Storage(_)
        | PersistenceError::OutcomeUnknown(_)
        | PersistenceError::StorageFull { .. } => EntityResponse::store_unavailable(state, said),
        PersistenceError::ConcurrencyViolation { .. } | PersistenceError::Serialization(_) => {
            EntityResponse::write_failed(state, said)
        }
    }
}

/// What a write that answered with an error did, as the journal shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WriteOutcome {
    /// The journal holds the event at `sequence_nr`.
    Landed { sequence_nr: u64 },
    /// The journal does not hold it, and never will.
    NotWritten,
    /// The journal cannot establish the outcome yet: why.
    Unknown(String),
}

/// Why a concurrency retry (ADR-0046) gave up without writing the action.
enum RetryGaveUp {
    /// The journal, read again, does not hold the action and it no longer
    /// applies: nothing was written.
    Refused(String),
    /// The journal kept moving, or it already holds the action: the action
    /// may have landed.
    WriteFailed(String),
    /// The journal, read again, does not hold the action, and the record is
    /// not the state the action's caller decided it on: nothing was written.
    RecordChanged,
    /// The store did not take a retry's event: answered as any write the
    /// store did not take (`persistence_failure_reply`).
    NotTaken(PersistenceError),
}

/// Why an action's field-overflow blobs, which its event refers to, were not
/// stored, so the event was not written either.
#[derive(Debug, thiserror::Error)]
enum BlobsNotStored {
    /// No object blob store is configured, so they cannot be stored at all.
    #[error("field-overflow blobs require a configured object blob store")]
    NoBlobStore,
    /// The blob store did not take them: the store's failure, answered as an
    /// event the store did not take is, so the caller may try again.
    #[error("{0}")]
    NotTaken(PersistenceError),
}

impl BlobsNotStored {
    /// The answer to the action whose blobs these are, its record in `state`.
    fn reply(self, state: EntityState) -> EntityResponse {
        match self {
            Self::NoBlobStore => EntityResponse::write_failed(state, self.to_string()),
            Self::NotTaken(error) => persistence_failure_reply(state, &error),
        }
    }
}

impl From<BlobsNotStored> for RetryGaveUp {
    fn from(not_stored: BlobsNotStored) -> Self {
        match not_stored {
            BlobsNotStored::NoBlobStore => Self::WriteFailed(not_stored.to_string()),
            BlobsNotStored::NotTaken(error) => Self::NotTaken(error),
        }
    }
}

/// The refusal an action gets when its record changed after it was decided.
const ACTION_RECORD_CHANGED: &str =
    "action authorization became stale; retry against current state";

pub(super) fn event_budget_workspace_id(state: &EntityState) -> String {
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
    params: &serde_json::Value,
    related: &temper_jit::table::RelatedMap,
) -> Vec<String> {
    if !table.composite_actions.contains_key(action) {
        return Vec::new();
    }

    let mut ctx = build_eval_context_with_xref(state, related, table, Some(action));
    super::effects::with_params(&mut ctx, params);
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

/// The refusal for a request sent with a key this entity already recorded for
/// a different request (`recorded` names that request's action).
fn idempotency_key_reused(state: &EntityState, recorded: &str) -> EntityResponse {
    EntityResponse {
        refusal: Some(Refusal::IdempotencyKeyReused {
            action: recorded.to_string(),
        }),
        ..EntityResponse::refused(
            state.clone(),
            format!(
                "this Idempotency-Key was already used on this entity for a different request \
                 (action `{recorded}`); send a new key for a new request"
            ),
        )
    }
}

/// The entity actor -- processes actions through a TransitionTable.
/// Optionally persists events to the configured backend. Wide events are emitted
/// via the OTEL SDK (no-op when OTEL is not initialised).
pub struct EntityActor {
    pub(super) tenant: String,
    pub(super) entity_type: String,
    entity_id: String,
    /// Live reference to the transition table. Reads through `RwLock` so that
    /// hot-swapped tables are visible on the next action dispatch without
    /// restarting the actor.
    pub(super) table: Arc<RwLock<TransitionTable>>,
    pub(super) initial_fields: serde_json::Value,
    /// Optional event journal for persistence. None = in-memory only.
    pub(super) event_journal: Option<BoxedEventStore>,
    /// Optional async snapshot writer. Event appends remain synchronous.
    pub(super) snapshot_queue: Option<Arc<SnapshotWriteQueue>>,
    /// Persistence backend label used for metrics and backend-specific field sync.
    pub(super) event_backend: Option<BackendLabel>,
    /// Trace ID for correlating all events from this actor.
    trace_id: String,
    /// Shared idempotency cache (ADR-0048 sub-decision 5). Consulted before
    /// executing an action whose `idempotency_key` is set, so dispatch-layer
    /// retries that race past the caller's timeout cannot double-execute.
    idempotency_cache: Option<Arc<crate::idempotency::IdempotencyCache>>,
    /// Object store for field-overflow blob bytes. SQL stores only refs.
    pub(super) blob_store: Option<crate::blob_store::BlobStore>,
    legacy_blob_store: Option<Arc<dyn crate::storage::BlobStore>>,
    /// Run once with the state this actor rehydrated from stored events.
    on_rehydrated: Option<RehydrateHook>,
    /// A write's outcome could not be learned, so the state in memory may not
    /// be the journal's. Once the message that wrote it is answered, the
    /// actor restarts and rebuilds from the journal before the next one.
    rebuild_from_journal: AtomicBool,
    /// What the current message is awaiting, for the watchdog's reports.
    pub(super) now: super::watchdog::Now,
    /// Per-attempt ask deadline: when startup and message reports begin.
    action_dispatch_timeout: Duration,
    /// Says which query-plane and timer rows an event writes, for a store
    /// that keeps them in the journal's transaction.
    entity_rows: Option<EntityRowsHook>,
    /// Who creates the record, when this actor writes its `Created` event:
    /// the caller its first state's timer row is armed as, in that commit.
    created_by: Option<AgentContext>,
}

/// What an entity actor runs when it starts from stored events, with the
/// state it rehydrated into, before it takes any message.
pub(crate) type RehydrateHook = Arc<dyn Fn(&EntityState) + Send + Sync>;

impl EntityActor {
    pub(crate) fn build_initial_state(
        entity_type: &str,
        entity_id: &str,
        table: &TransitionTable,
        initial_fields: &serde_json::Value,
    ) -> EntityState {
        let mut fields = initial_fields.clone();
        let mut counters = BTreeMap::new();
        let mut booleans = BTreeMap::new();
        table.initialize_declared_fields(&mut fields, &mut counters, &mut booleans);
        super::effects::canonicalize_entity_fields(&mut fields, entity_id, &table.initial_state);

        EntityState {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            status: table.initial_state.clone(),
            item_count: 0,
            counters,
            booleans,
            lists: if table.has_input_contracts() {
                table.initial_values.lists.clone()
            } else {
                BTreeMap::new()
            },
            fields,
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: BTreeMap::new(),
            last_event: Default::default(),
            status_clock: Default::default(),
            spawn_reservations: Default::default(),
        }
    }

    /// Snapshot frequency in events.
    ///
    /// Controlled by `TEMPER_SNAPSHOT_INTERVAL` (default 100).
    pub(super) fn snapshot_interval() -> u64 {
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
            legacy_blob_store: None,
            on_rehydrated: None,
            rebuild_from_journal: AtomicBool::new(false),
            now: super::watchdog::Now::default(),
            action_dispatch_timeout: crate::state::DEFAULT_ACTION_DISPATCH_TIMEOUT,
            entity_rows: None,
            created_by: None,
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
            legacy_blob_store: None,
            on_rehydrated: None,
            rebuild_from_journal: AtomicBool::new(false),
            now: super::watchdog::Now::default(),
            action_dispatch_timeout: crate::state::DEFAULT_ACTION_DISPATCH_TIMEOUT,
            entity_rows: None,
            created_by: None,
        }
    }

    /// Set the tenant for this actor (must be called before spawning).
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// Use the dispatcher's per-attempt ask deadline for startup and message reports.
    pub(crate) fn with_action_dispatch_timeout(mut self, timeout: Duration) -> Self {
        self.action_dispatch_timeout = timeout;
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
    pub(crate) fn with_legacy_blob_store(
        mut self,
        store: Option<Arc<dyn crate::storage::BlobStore>>,
    ) -> Self {
        self.legacy_blob_store = store;
        self
    }

    /// Attach the hook run when this actor rehydrates from stored events.
    pub(crate) fn with_on_rehydrated(mut self, hook: Option<RehydrateHook>) -> Self {
        self.on_rehydrated = hook;
        self
    }

    /// Attach what says which query-plane and timer rows an event writes.
    pub(crate) fn with_entity_rows(mut self, hook: Option<EntityRowsHook>) -> Self {
        self.entity_rows = hook;
        self
    }

    /// The record's creator, for a `Created` event this actor writes.
    pub(crate) fn with_creator(mut self, created_by: Option<AgentContext>) -> Self {
        self.created_by = created_by;
        self
    }

    pub(crate) fn with_blob_store(
        mut self,
        blob_store: Option<crate::blob_store::BlobStore>,
    ) -> Self {
        self.blob_store = blob_store;
        self
    }

    /// Store the field-overflow blobs an event refers to, before the event.
    async fn persist_overflow_blobs(
        blob_store: Option<&crate::blob_store::BlobStore>,
        blobs: &[crate::blobs::OverflowBlobWrite],
    ) -> Result<(), BlobsNotStored> {
        let Some(blob_store) = blob_store else {
            return Err(BlobsNotStored::NoBlobStore);
        };
        crate::blobs::put_overflow_blobs(blob_store, blobs)
            .await
            .map_err(|error| {
                BlobsNotStored::NotTaken(PersistenceError::Storage(format!(
                    "field-overflow blob persistence failed: {error}"
                )))
            })
    }

    /// Persistence ID for this entity: "tenant:EntityType:EntityId".
    pub(super) fn persistence_id(&self) -> String {
        format!("{}:{}:{}", self.tenant, self.entity_type, self.entity_id)
    }

    /// Storage transformation shared by execution and outside-write previews.
    pub(crate) fn field_sync_mode_for_backend(
        backend: Option<BackendLabel>,
        blob_store: Option<&crate::blob_store::BlobStore>,
    ) -> FieldSyncMode {
        match backend {
            Some(BackendLabel::Turso | BackendLabel::TursoRouted) => {
                FieldSyncMode::blob_refs_default()
            }
            Some(_) if blob_store.is_some() => FieldSyncMode::blob_refs_default(),
            _ => FieldSyncMode::InlineTruncate,
        }
    }

    /// Persist an event to the configured event store, with the rows of the
    /// scheduled actions it arms and the removal of the one it fires in the
    /// same commit, and, for a store that keeps them there, the record's
    /// query-plane row and timer row as the event leaves it (`caller` is who
    /// a timer it arms fires as). `state` is the record after the event, at
    /// the sequence before it.
    ///
    /// The answer is what the write did. `Ok` is the event's sequence: it
    /// landed, also when the store answered an error and the journal, read
    /// again, holds the event. A concurrency violation or a `not written`
    /// storage error: it did not land. An `outcome unknown` error: the
    /// journal cannot yet establish whether the failed commit landed.
    pub(super) async fn persist_event(
        &self,
        store: &BoxedEventStore,
        backend: BackendLabel,
        state: &mut EntityState,
        event: &EntityEvent,
        scheduled: &ScheduledRows,
        caller: Option<&AgentContext>,
    ) -> Result<u64, PersistenceError> {
        let persistence_id = self.persistence_id();
        let event_id = sim_uuid();
        let entity_rows = match &self.entity_rows {
            Some(hook) if store.commits_entity_rows() => {
                // The record as the event leaves it. A delete sets its status
                // only once its tombstone is written, so the status is the
                // event's.
                let mut after = state.clone();
                after.sequence_nr = state.sequence_nr + 1;
                if !event.to_status.is_empty() {
                    after.status = event.to_status.clone();
                }
                after.push_event_bounded(event.clone());
                hook(&RowsInput {
                    state: &after,
                    event,
                    caller,
                })
            }
            _ => EntityRows::default(),
        };
        let payload = super::bootstrap::event_payload(event, state)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let envelope = PersistenceEnvelope {
            sequence_nr: state.sequence_nr + 1,
            event_type: event.action.clone(),
            payload,
            metadata: EventMetadata {
                event_id,
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: event.timestamp,
                actor_id: persistence_id.clone(),
            },
        };

        // W2 / temper#146: measure append wait — the hypothesis is that
        // writer-lock / fsync serialization is a cold-start bottleneck.
        // ADR-0153/0155: derive the declared key rows AND the vector-index rows from
        // the new state and co-commit them with the journal append, so a keyed read
        // is correct without a scan and a kNN read reflects the write deterministically.
        let (key_rows, vector_rows, reconcile_vectors) = {
            let table = self.table.read().expect("table lock poisoned");
            // The type declares vector paths → the store reconciles this entity's
            // vector rows (delete stale + insert current) even when no row is emitted
            // this write (a delete transition or a cleared property), so stale rows are
            // purged instead of being ranked forever (ADR-0155).
            let reconcile_vectors = !table.vectors.is_empty();
            let mut key_rows = Vec::new();
            let mut vector_rows = Vec::new();
            if let Some(field_map) = state.fields.as_object() {
                for key in &table.keys {
                    if let Some(hash) =
                        crate::key_index::canonical_key_hash(&key.name, &key.properties, field_map)
                    {
                        key_rows.push(temper_runtime::persistence::EntityKeyRow {
                            key_name: key.name.clone(),
                            key_hash: hash,
                        });
                    }
                }
                // A soft-deleted (tombstone) entity is never indexed — it emits no
                // vector rows, so the reconcile below PURGES any it had, even though
                // its embedding field may still be present. Mirrors how the field-index
                // projection removes a deleted entity.
                let index_vectors = state.status != "Deleted";
                for decl in table.vectors.iter().filter(|_| index_vectors) {
                    // A vector is indexed only when its property parses to `dims`
                    // floats AND its model tag is a non-empty string — otherwise the
                    // path indexes nothing for this entity (like an incomplete key).
                    let Some(vector) = field_map
                        .get(&decl.property)
                        .and_then(|v| crate::vector_index::parse_vector_property(v, decl.dims))
                    else {
                        continue;
                    };
                    let Some(model_tag) = field_map
                        .get(&decl.model_property)
                        .and_then(|v| v.as_str())
                        .filter(|tag| !tag.is_empty())
                    else {
                        continue;
                    };
                    vector_rows.push(temper_runtime::persistence::EntityVectorRow {
                        decl_name: decl.name.clone(),
                        model_tag: model_tag.to_string(),
                        vector,
                    });
                }
            }
            (key_rows, vector_rows, reconcile_vectors)
        };
        let append_start = Instant::now(); // determinism-ok: production-only event-store wait metric
        let result = self
            .now
            .during(
                Doing::Appending,
                store.append_with_rows(
                    &persistence_id,
                    state.sequence_nr,
                    &[envelope],
                    entity_rows.commit_rows(CommitRows {
                        key_rows: &key_rows,
                        vector_rows: &vector_rows,
                        reconcile_vectors,
                        scheduled_actions: &scheduled.armed,
                        fired_scheduled_action: scheduled.fired.as_deref(),
                        projection: None,
                        state_timer: None,
                    }),
                ),
            )
            .await;
        crate::runtime_metrics::record_event_store_append_wait(
            backend.as_str(),
            "append",
            append_start.elapsed(),
        );
        let error = match result {
            Ok(new_seq) => {
                state.sequence_nr = new_seq;
                tracing::debug!(entity = %state.entity_id, seq = new_seq, "event persisted");
                return Ok(new_seq);
            }
            // Refused before writing: nothing to learn. The event store logged
            // a refusal for size once, with its words, and a full disk once
            // when it began refusing.
            Err(
                e @ (PersistenceError::EventTooLarge { .. }
                | PersistenceError::Serialization(_)
                | PersistenceError::StorageFull { .. }),
            ) => {
                return Err(e);
            }
            Err(e) => e,
        };
        // Any other answer can hide a write that landed: a commit whose answer
        // was lost, or a store that retried one and met its own event as a
        // concurrency violation. An unknown commit may still be running;
        // journal absence cannot turn that into a definite failure.
        let outcome = self
            .now
            .during(
                Doing::LearningWriteOutcome,
                Self::learn_write_outcome(
                    store,
                    &persistence_id,
                    state.sequence_nr,
                    event_id,
                    &error,
                ),
            )
            .await;
        self.log_write_outcome(&event.action, &outcome, &error);
        match outcome {
            WriteOutcome::Landed { sequence_nr } => {
                state.sequence_nr = sequence_nr;
                Ok(sequence_nr)
            }
            WriteOutcome::NotWritten => match error {
                PersistenceError::ConcurrencyViolation { .. } => Err(error),
                error => Err(PersistenceError::Storage(format!("not written: {error}"))),
            },
            WriteOutcome::Unknown(reason) => {
                self.rebuild_from_journal.store(true, Ordering::Release);
                Err(PersistenceError::OutcomeUnknown(format!(
                    "{error}; the journal cannot establish the outcome: {reason}"
                )))
            }
        }
    }

    /// What a write of `event_id` after `from_sequence` that answered with an
    /// error did, read from the journal.
    async fn learn_write_outcome(
        store: &BoxedEventStore,
        persistence_id: &str,
        from_sequence: u64,
        event_id: uuid::Uuid,
        append_error: &PersistenceError,
    ) -> WriteOutcome {
        match store.read_events(persistence_id, from_sequence).await {
            Ok(events) => match events
                .iter()
                .find(|event| event.metadata.event_id == event_id)
            {
                Some(event) => WriteOutcome::Landed {
                    sequence_nr: event.sequence_nr,
                },
                None if matches!(append_error, PersistenceError::OutcomeUnknown(_)) => {
                    WriteOutcome::Unknown(
                        "the event is absent but its commit may still finish".into(),
                    )
                }
                None => WriteOutcome::NotWritten,
            },
            Err(error) => WriteOutcome::Unknown(error.to_string()),
        }
    }

    /// Log what a write whose store answered `error` did, once it is known:
    /// the entity, its type and the action, never the action's parameters.
    fn log_write_outcome(&self, action: &str, outcome: &WriteOutcome, error: &PersistenceError) {
        let conflict = matches!(error, PersistenceError::ConcurrencyViolation { .. });
        match outcome {
            WriteOutcome::Landed { sequence_nr } => tracing::warn!(
                tenant = %self.tenant, entity_type = %self.entity_type,
                entity_id = %self.entity_id, action, sequence_nr, error = %error,
                "event landed: its write answered an error, and the journal holds it"
            ),
            // A conflict is the record moving on; the caller replays it.
            WriteOutcome::NotWritten if conflict => tracing::debug!(
                tenant = %self.tenant, entity_type = %self.entity_type,
                entity_id = %self.entity_id, action, error = %error,
                "event not written: the journal moved on"
            ),
            WriteOutcome::NotWritten => tracing::error!(
                tenant = %self.tenant, entity_type = %self.entity_type,
                entity_id = %self.entity_id, action, error = %error,
                "event rolled back: not written, the record keeps its state"
            ),
            WriteOutcome::Unknown(reason) => tracing::error!(
                tenant = %self.tenant, entity_type = %self.entity_type,
                entity_id = %self.entity_id, action, error = %error,
                reason = %reason,
                "event outcome unknown: the journal cannot establish whether the write landed"
            ),
        }
    }

    /// Save a snapshot when the configured interval is reached.
    pub(super) async fn maybe_save_snapshot(
        store: &BoxedEventStore,
        snapshot_queue: Option<&Arc<SnapshotWriteQueue>>,
        persistence_id: &str,
        state: &mut EntityState,
    ) -> Result<(), PersistenceError> {
        if state.sequence_nr == 0 {
            return Ok(());
        }
        if let Some(queue) = snapshot_queue
            && let Some(applied_sequence) = queue.applied_sequence(persistence_id)
            && applied_sequence > state.last_snapshot_sequence_nr
        {
            let applied_sequence = applied_sequence.min(state.sequence_nr);
            state.last_snapshot_sequence_nr = applied_sequence;
            state.events_since_snapshot =
                state.sequence_nr.saturating_sub(applied_sequence) as usize;
        }

        let interval = Self::snapshot_interval();
        let pending_sequence = snapshot_queue
            .and_then(|queue| queue.pending_sequence(persistence_id))
            .unwrap_or(0);
        let latest_snapshot_boundary = state.last_snapshot_sequence_nr.max(pending_sequence);
        if state.sequence_nr.saturating_sub(latest_snapshot_boundary) < interval {
            return Ok(());
        }

        let snapshot = Self::serialize_snapshot_state(state)?;
        if let Some(queue) = snapshot_queue {
            match queue.enqueue(persistence_id.to_string(), state.sequence_nr, snapshot) {
                SnapshotEnqueueOutcome::Enqueued
                | SnapshotEnqueueOutcome::Coalesced
                | SnapshotEnqueueOutcome::StaleSkipped => return Ok(()),
                SnapshotEnqueueOutcome::Full => {
                    tracing::warn!(
                        entity = %state.entity_id,
                        seq = state.sequence_nr,
                        "snapshot write queue full; keeping replay tail open"
                    );
                    return Ok(());
                }
            }
        }

        store
            .save_snapshot(persistence_id, state.sequence_nr, &snapshot)
            .await?;
        state.last_snapshot_sequence_nr = state.sequence_nr;
        state.events_since_snapshot = 0;
        Ok(())
    }

    /// Replay events from the configured store to rebuild state (called in pre_start).
    ///
    /// Each event restores what it committed: its stored `to_status`, the
    /// outcome its effects recorded (counters, booleans, lists, and fields such
    /// as a spawned child's id), and its params. Replay mints, spawns, schedules
    /// and calls out nothing. Events journaled before outcomes were recorded
    /// re-derive their effects from the `TransitionTable`, minting nothing.
    pub(super) async fn replay_events(
        table: &TransitionTable,
        store: &BoxedEventStore,
        backend: BackendLabel,
        state: &mut EntityState,
        tenant: &str,
        blob_store: Option<&crate::blob_store::BlobStore>,
        replay_options: (ReplayPolicy, ReplayLogging),
    ) -> Result<(), ActorError> {
        let (replay_policy, logging) = replay_options;
        // Keep failures visible for both callers; only successful replay
        // diagnostics are reduced during a record walk.
        macro_rules! replay_log {
            ($($fields:tt)*) => {
                match logging {
                    ReplayLogging::Actor => tracing::info!($($fields)*),
                    ReplayLogging::Walk => tracing::debug!($($fields)*),
                }
            };
        }
        let replay_start = Instant::now(); // determinism-ok: wall-clock for production replay duration metric only
        let persistence_id = format!("{tenant}:{}:{}", state.entity_type, state.entity_id);
        let persistence_id = persistence_id.as_str();
        let mut from_sequence = 0;
        let mut loaded_snapshot = false;

        if replay_policy.loads_snapshot() {
            match store.load_snapshot(persistence_id).await {
                Ok(Some((snapshot_seq, snapshot_bytes))) => {
                    if Self::apply_snapshot_bytes(state, snapshot_seq, &snapshot_bytes) {
                        from_sequence = snapshot_seq;
                        loaded_snapshot = true;
                        replay_log!(
                            entity = %state.entity_id,
                            seq = snapshot_seq,
                            "loaded snapshot before replay"
                        );
                    } else {
                        tracing::warn!(
                            entity = %state.entity_id,
                            seq = snapshot_seq,
                            "failed to deserialize snapshot, falling back to full replay"
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        entity = %state.entity_id,
                        error = %e,
                        "failed to load snapshot, falling back to full replay"
                    );
                }
            }
        }

        match store.read_events(persistence_id, from_sequence).await {
            Ok(envelopes) => {
                if envelopes.len() > MAX_EVENTS_SINCE_SNAPSHOT {
                    return Err(ActorError::custom(format!(
                        "snapshot tail replay budget exceeded for {}:{} ({} > {} events since snapshot)",
                        state.entity_type,
                        state.entity_id,
                        envelopes.len(),
                        MAX_EVENTS_SINCE_SNAPSHOT
                    )));
                }
                if !loaded_snapshot && from_sequence == 0 && !envelopes.is_empty() {
                    super::bootstrap::clear_for_replay(state, &table.initial_state);
                }
                let (first_event_at, first_origin) = super::last_event::creation(
                    store,
                    persistence_id,
                    &state.last_event,
                    &envelopes,
                    loaded_snapshot,
                )
                .await?;
                let mut expected_sequence = from_sequence.saturating_add(1);
                for (index, env) in envelopes.iter().enumerate() {
                    if replay_policy.strict_event_validation() {
                        if env.sequence_nr != expected_sequence {
                            return Err(ActorError::custom(format!(
                                "non-contiguous journal for {}:{}: expected sequence {}, found {}",
                                state.entity_type,
                                state.entity_id,
                                expected_sequence,
                                env.sequence_nr
                            )));
                        }
                        expected_sequence = env.sequence_nr.checked_add(1).ok_or_else(|| {
                            ActorError::custom(format!(
                                "journal sequence overflow for {}:{}",
                                state.entity_type, state.entity_id
                            ))
                        })?;
                        if env.metadata.actor_id != persistence_id {
                            return Err(ActorError::custom(format!(
                                "journal event for {}:{} at sequence {} is bound to actor '{}'",
                                state.entity_type,
                                state.entity_id,
                                env.sequence_nr,
                                env.metadata.actor_id
                            )));
                        }
                    }

                    if env.event_type == COMPOSITE_EVENT_TYPE {
                        if replay_policy.strict_event_validation() {
                            super::replay_validation::validate_strict_composite_event(
                                tenant, state, env,
                            )?;
                        }
                        state.sequence_nr = env.sequence_nr;
                        continue;
                    }

                    let parsed_event = serde_json::from_value::<EntityEvent>(env.payload.clone());

                    // The kernel's tombstone is terminal: once deleted, an entity
                    // must not replay into a live state. A spec action that shares
                    // the tombstone's name replays like any other action.
                    if env.is_tombstone() {
                        let tombstone = match parsed_event {
                            Ok(mut event) => {
                                if replay_policy.strict_event_validation() {
                                    super::replay_validation::validate_strict_tombstone(
                                        state, env, &event,
                                    )?;
                                }
                                event.params =
                                    super::effects::sanitize_action_params(&event.params)
                                        .into_owned();
                                event.kind = temper_runtime::persistence::EventKind::Tombstone;
                                event
                            }
                            Err(error) if replay_policy.strict_event_validation() => {
                                return Err(ActorError::custom(format!(
                                    "invalid tombstone event for {}:{} at sequence {}: {error}",
                                    state.entity_type, state.entity_id, env.sequence_nr
                                )));
                            }
                            Err(_) => EntityEvent {
                                action: "Deleted".to_string(),
                                from_status: state.status.clone(),
                                to_status: "Deleted".to_string(),
                                timestamp: env.metadata.timestamp,
                                params: serde_json::json!({}),
                                idempotency_key: None,
                                origin: Default::default(),
                                kind: temper_runtime::persistence::EventKind::Tombstone,
                                outcome: None,
                            },
                        };
                        state.record_tombstone(tombstone);
                        state.sequence_nr = env.sequence_nr;
                        if replay_policy.strict_event_validation() && index + 1 != envelopes.len() {
                            return Err(ActorError::custom(format!(
                                "journal for {}:{} contains events after terminal tombstone at sequence {}",
                                state.entity_type, state.entity_id, env.sequence_nr
                            )));
                        }
                        break;
                    }

                    // A refused spawn is the kernel's event too: it changes only
                    // the field that named the child, through the live helper. So
                    // is a stopped timer, which only says so (`LastEvent::record`).
                    if env.event_type == super::spawn_refusal::SPAWN_REFUSED_EVENT
                        || env.event_type == super::timer_needs_person::TIMER_NEEDS_PERSON_EVENT
                    {
                        match parsed_event {
                            Ok(event) => {
                                if env.event_type == super::spawn_refusal::SPAWN_REFUSED_EVENT {
                                    super::spawn_refusal::apply(state, &event.params);
                                }
                                state.push_event_bounded(event);
                            }
                            Err(e) => {
                                if replay_policy.strict_event_validation() {
                                    return Err(ActorError::custom(format!(
                                        "invalid spawn-refusal event for {}:{} at sequence {}: {e}",
                                        state.entity_type, state.entity_id, env.sequence_nr
                                    )));
                                }
                                tracing::warn!(
                                    entity = %state.entity_id,
                                    sequence_nr = env.sequence_nr,
                                    error = %e,
                                    "skipping spawn-refusal event with incompatible schema during replay"
                                );
                            }
                        }
                        state.sequence_nr = env.sequence_nr;
                        continue;
                    }

                    // PATCH/PUT field updates are journaled outside the spec's
                    // action vocabulary (ARN-189). Re-apply them through the
                    // same helper the live handler uses so a rehydrated entity
                    // reaches exactly the live post-update state — including
                    // PUT's replace semantics, which the generic param-sync
                    // path below cannot express (it only merges).
                    if env.event_type == super::effects::FIELDS_UPDATED_EVENT
                        || env.event_type == super::effects::FIELDS_REPLACED_EVENT
                    {
                        match parsed_event {
                            Ok(event) => {
                                let applied = super::effects::apply_field_update(
                                    state,
                                    &event.params,
                                    env.event_type == super::effects::FIELDS_REPLACED_EVENT,
                                );
                                if !applied {
                                    // A journaled field update whose payload is not
                                    // an object — only reachable from a build that
                                    // predates the live guard. It is as dropped as
                                    // one that failed to deserialize, so it fails
                                    // or counts the same way.
                                    if replay_policy.strict_event_validation() {
                                        return Err(ActorError::custom(format!(
                                            "non-object field-update event for {}:{} at sequence {}",
                                            state.entity_type, state.entity_id, env.sequence_nr
                                        )));
                                    }
                                    crate::event_budget_metrics::record_field_update_replay_skip(
                                        tenant,
                                        &state.entity_type,
                                        &state.entity_id,
                                    );
                                }
                                state.push_event_bounded(event);
                            }
                            Err(e) => {
                                // Honor the replay policy, like the tombstone and
                                // generic arms do. Under a strict policy the caller
                                // is resolving authoritative state — identity and
                                // authority decisions read from it — so silently
                                // dropping a field update there can preserve
                                // exactly the authority a `FieldsReplaced` was
                                // meant to revoke. Fail instead of skipping.
                                if replay_policy.strict_event_validation() {
                                    return Err(ActorError::custom(format!(
                                        "invalid field-update event for {}:{} at sequence {}: {e}",
                                        state.entity_type, state.entity_id, env.sequence_nr
                                    )));
                                }
                                crate::event_budget_metrics::record_field_update_replay_skip(
                                    tenant,
                                    &state.entity_type,
                                    &state.entity_id,
                                );
                                tracing::warn!(
                                    entity = %state.entity_id,
                                    sequence_nr = env.sequence_nr,
                                    event_type = %env.event_type,
                                    error = %e,
                                    "skipping field-update event with incompatible schema during replay"
                                );
                            }
                        }
                        state.sequence_nr = env.sequence_nr;
                        continue;
                    }

                    match parsed_event {
                        Ok(mut event) => {
                            if replay_policy.strict_event_validation() {
                                super::replay_validation::validate_strict_entity_event(
                                    table, state, env, &event,
                                )?;
                            }
                            let frozen_bootstrap = event.action == "Created"
                                && event.from_status.is_empty()
                                && env.payload.get("initial_values").is_some();
                            if frozen_bootstrap {
                                if env.sequence_nr != 1 || state.total_event_count != 0 {
                                    return Err(ActorError::custom(
                                        "initial values require the first bootstrap event",
                                    ));
                                }
                                super::bootstrap::restore(state, &env.payload)?;
                            }
                            event.params =
                                super::effects::sanitize_action_params(&event.params).into_owned();
                            // A persisted event is a historical fact: its guard
                            // already passed, and its stored outcome and target
                            // status are authoritative. Never re-run recorded
                            // effects against the active spec or mint child ids.
                            // Legacy events select effects using the stored
                            // from-status so earlier replay drift cannot prevent
                            // later events from matching their original rule.
                            let rule_from = if event.from_status.is_empty() {
                                state.status.as_str()
                            } else {
                                event.from_status.as_str()
                            };
                            if !frozen_bootstrap && let Some(outcome) = event.outcome.as_ref() {
                                outcome.restore(state);
                            } else if !frozen_bootstrap
                                && let Some(effects) =
                                    table.replay_effects(rule_from, &event.action)
                            {
                                // Events predating outcomes re-derive effects,
                                // ignoring guards and never minting child ids.
                                let effects = effects.to_vec();
                                super::effects::apply_legacy_replay_effects(
                                    state,
                                    &effects,
                                    &event.params,
                                );
                            }
                            // The stored target always wins over the active
                            // spec's SetState, including for legacy events.
                            if !event.to_status.is_empty() {
                                state.status.clone_from(&event.to_status);
                            }

                            // Sync action params into fields — mirrors the live
                            // process_action() path (effects.rs:155) so data fields
                            // like Title, Description, Priority survive replay.
                            let field_sync_mode =
                                Self::field_sync_mode_for_backend(Some(backend), blob_store);
                            let overflow_blobs = if frozen_bootstrap {
                                Vec::new()
                            } else {
                                super::effects::sync_fields_with_metadata(
                                    state,
                                    &event.params,
                                    field_sync_mode,
                                    Some(&table.state_var_metadata),
                                )
                            };
                            // Persist replayed overflow blobs so blob-ref envelopes
                            // resolve on subsequent OData reads. Content-addressed
                            // dedup makes this idempotent — if the original live
                            // action already persisted the blob, INSERT OR IGNORE
                            // is a no-op. If the prior server died between emitting
                            // the event and persisting the blob, this is the
                            // recovery path. See ADR-0040, ADR-0045.
                            if !overflow_blobs.is_empty()
                                && let Err(e) =
                                    Self::persist_overflow_blobs(blob_store, &overflow_blobs).await
                            {
                                tracing::warn!(
                                    entity = %state.entity_id,
                                    error = %e,
                                    overflow_count = overflow_blobs.len(),
                                    "failed to persist replayed overflow blobs — blob-ref envelopes may dangle"
                                );
                            }

                            state.push_event_bounded(event);
                        }
                        Err(e) => {
                            if replay_policy.strict_event_validation() {
                                return Err(ActorError::custom(format!(
                                    "invalid event for {}:{} at sequence {}: {e}",
                                    state.entity_type, state.entity_id, env.sequence_nr
                                )));
                            }
                            // Schema-mismatched event: log and skip rather than panic.
                            // This preserves entity hydration across spec evolution —
                            // the last valid state is used and replay continues.
                            tracing::warn!(
                                entity = %state.entity_id,
                                event_id = %env.metadata.event_id,
                                sequence_nr = env.sequence_nr,
                                event_type = %env.event_type,
                                error = %e,
                                "skipping event with incompatible schema during replay"
                            );
                            tracing::warn!(tenant = %tenant, entity_type = %state.entity_type, "event replay error");
                        }
                    }
                    state.sequence_nr = env.sequence_nr;
                }
                state.last_event.first_event_at = first_event_at;
                if let Some(origin) = &first_origin {
                    state.last_event.record_first(origin);
                }
                if !envelopes.is_empty() {
                    let replayed_tail = state
                        .sequence_nr
                        .saturating_sub(state.last_snapshot_sequence_nr)
                        as usize;
                    if replayed_tail > MAX_EVENTS_SINCE_SNAPSHOT {
                        tracing::error!(
                            entity = %state.entity_id,
                            replayed_tail,
                            cap = MAX_EVENTS_SINCE_SNAPSHOT,
                            "snapshot tail exceeds bounded replay cap"
                        );
                    }
                    replay_log!(
                        entity = %state.entity_id,
                        snapshot_loaded = loaded_snapshot,
                        replayed = envelopes.len(),
                        status = %state.status,
                        seq = state.sequence_nr,
                        total_events = state.total_event_count,
                        events_since_snapshot = state.events_since_snapshot,
                        recent_events = state.events.len(),
                        counters = ?state.counters,
                        booleans = ?state.booleans,
                        "state rebuilt from event journal via TransitionTable"
                    );
                } else if loaded_snapshot {
                    replay_log!(
                        entity = %state.entity_id,
                        seq = state.sequence_nr,
                        total_events = state.total_event_count,
                        events_since_snapshot = state.events_since_snapshot,
                        "state restored from snapshot (no delta events)"
                    );
                }
            }
            // A journal that could not be read is never replaced by the
            // snapshot or the initial state: either would answer with state
            // older than the journal. The caller fails and the next message
            // reads the journal again.
            Err(e) => {
                return Err(ActorError::custom(format!(
                    "failed to read events for replay of {}:{}: {e}",
                    state.entity_type, state.entity_id
                )));
            }
        }
        crate::runtime_metrics::record_event_replay_duration(
            replay_start.elapsed(),
            tenant,
            &state.entity_type,
        );
        Ok(())
    }
}

/// Rebuild an entity's current state from its snapshot + event tail.
///
/// A journal read failure is returned as an error, never folded into the
/// snapshot or initial state. Actor hydration then fails its first message
/// and the next message hydrates again; the backfills can tell "no events"
/// apart from "could not read the journal" (ADR-0153).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn recover_entity_state_from_store(
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    store: &BoxedEventStore,
    backend: BackendLabel,
    initial_fields: &serde_json::Value,
    blob_store: Option<&crate::blob_store::BlobStore>,
) -> Result<EntityState, ActorError> {
    let mut state = EntityActor::build_initial_state(entity_type, entity_id, table, initial_fields);
    EntityActor::replay_events(
        table,
        store,
        backend,
        &mut state,
        tenant,
        blob_store,
        (ReplayPolicy::StrictSnapshot, ReplayLogging::Actor),
    )
    .await?;
    Ok(state)
}

/// Replay for a record walk without emitting per-record info diagnostics.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn recover_entity_state_for_walk(
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    store: &BoxedEventStore,
    backend: BackendLabel,
    initial_fields: &serde_json::Value,
    blob_store: Option<&crate::blob_store::BlobStore>,
) -> Result<EntityState, ActorError> {
    let mut state = EntityActor::build_initial_state(entity_type, entity_id, table, initial_fields);
    EntityActor::replay_events(
        table,
        store,
        backend,
        &mut state,
        tenant,
        blob_store,
        (ReplayPolicy::StrictSnapshot, ReplayLogging::Walk),
    )
    .await?;
    Ok(state)
}

/// Rebuild state from the complete durable journal, ignoring snapshots and
/// failing closed on read errors, sequence gaps, incompatible events, or
/// history after a terminal tombstone. Tests use it as the strict reference
/// a live actor's state must agree with. (Identity resolution used it on
/// every request until credentials were read as ordinary entities.)
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn recover_authoritative_entity_state_from_store(
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    store: &BoxedEventStore,
    backend: BackendLabel,
    initial_fields: &serde_json::Value,
    blob_store: Option<&crate::blob_store::BlobStore>,
) -> Result<EntityState, ActorError> {
    let mut state = EntityActor::build_initial_state(entity_type, entity_id, table, initial_fields);
    EntityActor::replay_events(
        table,
        store,
        backend,
        &mut state,
        tenant,
        blob_store,
        (ReplayPolicy::StrictFullJournal, ReplayLogging::Actor),
    )
    .await?;
    Ok(state)
}

impl EntityActor {
    /// This record, as the watchdog's reports name it.
    fn watched_record(&self) -> super::watchdog::Record<'_> {
        super::watchdog::Record {
            tenant: &self.tenant,
            entity_type: &self.entity_type,
            entity_id: &self.entity_id,
        }
    }

    /// What `pre_start` does: replay the journal, and write `Created` for a new record.
    async fn start(&self) -> Result<EntityState, ActorError> {
        // Snapshot the table for consistent startup (initial state + replay).
        // This is a cheap clone — TransitionTable is a few Vecs of strings.
        let table = self.table.read().expect("table lock poisoned").clone();

        let mut state = Self::build_initial_state(
            &self.entity_type,
            &self.entity_id,
            &table,
            &self.initial_fields,
        );

        // Replay events from Postgres to rebuild state (if persistence is configured).
        // Re-evaluates each event through the TransitionTable to reconstruct
        // all state variables (status, counters, booleans) — not just item_count.
        if let (Some(store), Some(backend)) = (self.event_journal.as_ref(), self.event_backend) {
            state = self
                .now
                .during(
                    Doing::Replaying,
                    recover_entity_state_from_store(
                        &self.tenant,
                        &self.entity_type,
                        &self.entity_id,
                        &table,
                        store,
                        backend,
                        &self.initial_fields,
                        self.blob_store.as_ref(),
                    ),
                )
                .await?;
            if state.sequence_nr > 0
                && let Some(hook) = self.on_rehydrated.as_ref()
            {
                hook(&state);
            }
        }

        // Persist a bootstrap Created event for first-time entities so initial
        // fields are durable and replayable.
        if self.event_journal.is_some() && state.sequence_nr == 0 && state.total_event_count == 0 {
            let initial_params =
                super::effects::sanitize_action_params(&self.initial_fields).into_owned();
            let created = EntityEvent {
                action: "Created".to_string(),
                from_status: String::new(),
                to_status: state.status.clone(),
                timestamp: sim_now(),
                params: initial_params,
                idempotency_key: None,
                origin: super::EventOrigin::from_caller(self.created_by.as_ref()),
                kind: temper_runtime::persistence::EventKind::Transition,
                outcome: None,
            };

            if let (Some(store), Some(backend)) = (self.event_journal.as_ref(), self.event_backend)
            {
                // Its creator is its caller: a first state with a timer
                // commits the timer's row with this event.
                self.persist_event(
                    store,
                    backend,
                    &mut state,
                    &created,
                    &ScheduledRows::default(),
                    self.created_by.as_ref(),
                )
                .await
                .map_err(|e| match e {
                    // The journal moved on, or the write may have landed: a
                    // restart replays the journal and finds out.
                    PersistenceError::ConcurrencyViolation { .. }
                    | PersistenceError::OutcomeUnknown(_) => ActorError::custom(format!(
                        "failed to persist bootstrap Created event for {}:{}: {}",
                        self.entity_type, self.entity_id, e
                    )),
                    // Not written, so the record is not created. A restart would
                    // only retry the write, which its creator can do once told.
                    e => ActorError::InitFailed(format!(
                        "{}:{} was not created: {e}",
                        self.entity_type, self.entity_id
                    )),
                })?;
            }
            state.push_event_bounded(created);
        }

        Ok(state)
    }
}

impl Actor for EntityActor {
    type Msg = EntityMsg;
    type State = EntityState;

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<Self::State, ActorError> {
        super::watchdog::watched(
            self.watched_record(),
            "start",
            self.action_dispatch_timeout,
            &self.now,
            Box::pin(self.start()),
        )
        .await
    }

    async fn handle(
        &self,
        msg: Self::Msg,
        state: &mut Self::State,
        ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        let message = msg.name().to_owned();
        let handled = super::watchdog::watched(
            self.watched_record(),
            &message,
            self.action_dispatch_timeout,
            &self.now,
            Box::pin(self.handle_message(msg, state, ctx)),
        )
        .await;
        // The message is answered. A write whose outcome could not be learned
        // leaves memory that may not be the journal's: restart, so the next
        // message is answered from the journal (`pre_start` replays it, and
        // fails until it can read it).
        if self.rebuild_from_journal.swap(false, Ordering::AcqRel) {
            tracing::warn!(
                tenant = %self.tenant, entity_type = %self.entity_type,
                entity_id = %self.entity_id,
                "rebuilding from the journal: a write's outcome could not be learned"
            );
            return Err(ActorError::custom(format!(
                "{}:{} rebuilds from its journal: a write's outcome could not be learned",
                self.entity_type, self.entity_id
            )));
        }
        handled
    }

    async fn post_stop(&self, state: Self::State, _ctx: &mut ActorContext<Self>) {
        tracing::info!(
            entity = %state.entity_id,
            status = %state.status,
            events_total = state.total_event_count,
            events_recent = state.events.len(),
            "entity actor stopped"
        );
    }
}

impl EntityActor {
    /// Handle one message (`Actor::handle` restarts the actor after it when
    /// a write's outcome could not be learned).
    async fn handle_message(
        &self,
        msg: EntityMsg,
        state: &mut EntityState,
        ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        match msg {
            EntityMsg::Action {
                name,
                params,
                related,
                idempotency_key: keyed,
                expected_authorization_precondition,
                caller,
            } => {
                // What a retry under the key must repeat to be answered by it.
                let request = keyed.as_ref().map(|key| key.request(&name, &params));
                let idempotency_key = keyed.map(crate::idempotency::IdempotencyKey::into_string);
                // Capture start time for span duration (DST-safe: sim_now()
                // returns logical clock in simulation, wall clock in production).
                let action_start = sim_now();
                // Wall-clock start for `temper_actor_ask_reply_latency_ms`.
                // Separate from `action_start` because metrics emission is
                // outside the DST boundary; using Instant here is safe.
                let ask_reply_start = Instant::now(); // determinism-ok: observability only

                // ARN-189: the field-update event names are reserved. Replay
                // dispatches them to `apply_field_update` before the generic
                // action path, so a spec action of the same name would be
                // hijacked on rehydration — its params would be merged into
                // fields and its transition never replayed. Reserving them "by
                // convention" is not a guarantee; refuse the collision here,
                // where a domain action first enters the actor.
                if name == super::effects::FIELDS_UPDATED_EVENT
                    || name == super::effects::FIELDS_REPLACED_EVENT
                    || name == super::spawn_refusal::SPAWN_REFUSED_EVENT
                    || name == super::timer_needs_person::TIMER_NEEDS_PERSON_EVENT
                {
                    ctx.reply(EntityResponse::refused(
                        state.clone(),
                        format!("action name `{name}` is reserved for events the kernel journals"),
                    ));
                    return Ok(());
                }

                // Snapshot the current table for this action dispatch.
                // On the next action, any hot-swapped table will be picked up.
                let table = self.table.read().expect("table lock poisoned").clone();

                // ADR-0048 sub-decision 5: actor-side idempotency dedup.
                // A dispatch-layer retry can produce a second `ask` after the
                // caller's budget expires while the first ask is still in
                // flight to this actor. Without this check, both asks would
                // execute. Here we consult the shared cache keyed on the
                // caller's `Idempotency-Key` before executing; on a hit, the
                // previously-computed response is returned as the reply.
                //
                // A key answers only a retry of the request it was first used
                // for: the cache compares what the key names (the action and
                // parameters, or a follow-up's cause), the durable key set the
                // action. Anything else sent with the key is refused, so it can
                // never report another request's success as its own.
                let actor_key = self.persistence_id();
                if let (Some(key), Some(request), Some(cache)) = (
                    idempotency_key.as_ref(),
                    request.as_ref(),
                    self.idempotency_cache.as_ref(),
                ) {
                    match cache.get(&actor_key, key, request) {
                        crate::idempotency::CachedReply::Replay(cached) => {
                            // The cache keeps replies without event parameters
                            // or the durable key set; the live state is the
                            // reply's state in full only while this retry's own
                            // event is still newest.
                            let newest_is_this_retry = crate::idempotency::live_state_is_the_reply(
                                state,
                                &cached.state,
                                key,
                            );
                            let reply = if newest_is_this_retry {
                                EntityResponse {
                                    state: state.clone(),
                                    ..*cached
                                }
                            } else {
                                *cached
                            };
                            ctx.reply(reply);
                            return Ok(());
                        }
                        crate::idempotency::CachedReply::OtherRequest(recorded) => {
                            ctx.reply(idempotency_key_reused(state, &recorded));
                            return Ok(());
                        }
                        crate::idempotency::CachedReply::Miss => {}
                    }
                }
                // A follow-up's retry is its cause again, whatever action it
                // names now: the duplicate below answers it.
                if let Some(key) = idempotency_key.as_deref()
                    && let Some(recorded) = state.processed_idempotency_keys.get(key)
                    && request
                        .as_ref()
                        .is_none_or(|request| request.repeats_action())
                    && !recorded.was_for(&name)
                {
                    let recorded = recorded.action.clone().unwrap_or_default();
                    ctx.reply(idempotency_key_reused(state, &recorded));
                    return Ok(());
                }
                if let Some(key) = idempotency_key.as_deref()
                    && state.has_processed_idempotency_key(key)
                {
                    // A follow-up's retry that names another action is
                    // answered by the one its key recorded.
                    let answered_action = state
                        .processed_idempotency_keys
                        .get(key)
                        .and_then(|recorded| recorded.action.clone())
                        .filter(|recorded| *recorded != name);
                    let custom_effects = duplicate_idempotency_custom_effects(
                        &table,
                        state,
                        answered_action.as_deref().unwrap_or(&name),
                        &params,
                        &related,
                    );
                    let mut response_state = state.clone();
                    if !custom_effects.is_empty() {
                        prune_transient_action_fields_from_state(&mut response_state);
                    }
                    ctx.reply(EntityResponse {
                        success: true,
                        refusal: None,
                        state: response_state,
                        error: None,
                        custom_effects,
                        scheduled_actions: vec![],
                        spawn_requests: vec![],
                        spec_governed: true,
                        refused: false,
                        answered_action,
                        trigger_failures: Vec::new(),
                        store_unavailable: false,
                        unrecorded_at: None,
                    });
                    return Ok(());
                }

                if let Some(expected) = expected_authorization_precondition.as_deref()
                    && super::effects::entity_authorization_precondition(state) != expected
                {
                    ctx.reply(EntityResponse::record_changed(
                        state.clone(),
                        ACTION_RECORD_CHANGED.to_string(),
                    ));
                    return Ok(());
                }

                // A tombstone is a valid runtime state, but not a spec transition
                // source. Reject new actions without turning a normal caller
                // error into an actor-stopping debug assertion.
                if state.status == "Deleted" {
                    ctx.reply(EntityResponse::refused(
                        state.clone(),
                        "cannot execute actions after entity deletion".to_string(),
                    ));
                    return Ok(());
                }

                // The status need not be a state of the spec: a record waits in
                // a state a new version dropped until its `[[carry]]` moves it
                // (temper-server's `spec_change`). Only a rule that starts from
                // the status can move it, so every other action is refused.

                // Exhaustion is a normal refusal in both debug and release.
                if state.events_since_snapshot >= MAX_EVENTS_SINCE_SNAPSHOT {
                    let workspace_id = event_budget_workspace_id(state);
                    crate::event_budget_metrics::record_exhausted(
                        &self.tenant,
                        &state.entity_type,
                        &state.entity_id,
                        &workspace_id,
                    );
                    tracing::warn!(
                        tenant = %self.tenant,
                        entity_type = %state.entity_type,
                        entity_id = %state.entity_id,
                        workspace_id = %workspace_id,
                        status = %state.status,
                        action = %name,
                        events_since_snapshot = state.events_since_snapshot,
                        total_event_count = state.total_event_count,
                        max_events_since_snapshot = MAX_EVENTS_SINCE_SNAPSHOT,
                        "Event budget exhausted (10000 max since snapshot)"
                    );
                    ctx.reply(EntityResponse::refused(
                        state.clone(),
                        format!(
                            "Event budget exhausted ({MAX_EVENTS_SINCE_SNAPSHOT} max since snapshot)"
                        ),
                    ));
                    return Ok(());
                }

                // A state timer's fire applies only at the journal sequence
                // its timer was armed at (`timer_fire`): past it, another
                // writer handled this arming or the record moved on.
                let timer_armed_seq =
                    super::timer_fire::armed_seq(caller.as_deref(), &self.tenant, state, &name);
                if let Some(error) = super::timer_fire::moved_past(timer_armed_seq, state) {
                    ctx.reply(EntityResponse::refused(state.clone(), error));
                    return Ok(());
                }

                // Captured BEFORE the action applies. The retry path (ADR-0046)
                // updates these in lockstep with replay so postconditions hold
                // across the race window.
                let mut event_count_before = state.total_event_count;
                let mut state_before = state.clone();
                let field_sync_mode =
                    Self::field_sync_mode_for_backend(self.event_backend, self.blob_store.as_ref());

                // `result` and `event` are `mut` so that a successful ADR-0046
                // retry can replace them with values re-evaluated against the
                // caught-up state. The downstream telemetry and reply use
                // whichever pair last succeeded in persist.
                let mut result = self
                    .now
                    .during(
                        Doing::ReadingBlobs,
                        process_action_with_blob_prestate(
                            state,
                            &table,
                            &name,
                            &params,
                            &related,
                            field_sync_mode,
                            crate::blobs::BlobReadSource::Staged {
                                store: self.blob_store.as_ref(),
                                legacy: self.legacy_blob_store.as_deref(),
                                blobs: &[],
                            },
                        ),
                    )
                    .await;

                if result.success {
                    // process_action returned a successful transition with event.
                    // Clone out so `result.event` stays populated for re-use if
                    // the retry path needs to re-emit (simplifies lifetime here).
                    let mut event = result
                        .event
                        .clone()
                        .expect("successful process_action always returns event"); // ci-ok: post-assertion, success guarantees Some
                    event.idempotency_key = idempotency_key.clone();
                    event.origin = super::EventOrigin::from_caller(caller.as_deref());

                    // An event too large to be written stores no overflow blobs first.
                    if !result.overflow_blobs.is_empty()
                        && let Some(store) = self.event_journal.as_ref()
                        && let Err(e) = super::bootstrap::event_payload(&event, state)
                            .map_err(|e| PersistenceError::Serialization(e.to_string()))
                            .and_then(|payload| {
                                store.check_event_size(
                                    &self.persistence_id(),
                                    &event.action,
                                    &payload,
                                    false,
                                )
                            })
                    {
                        *state = state_before;
                        ctx.reply(persistence_failure_reply(state.clone(), &e));
                        return Ok(());
                    }

                    if !result.overflow_blobs.is_empty()
                        && let Err(e) = self
                            .now
                            .during(
                                Doing::WritingBlobs,
                                Self::persist_overflow_blobs(
                                    self.blob_store.as_ref(),
                                    &result.overflow_blobs,
                                ),
                            )
                            .await
                    {
                        *state = state_before;
                        ctx.reply(e.reply(state.clone()));
                        return Ok(());
                    }

                    // Persist to Postgres (if configured). On
                    // `ConcurrencyViolation` enter the ADR-0046 retry cycle —
                    // replay events, re-evaluate the action against the caught-up
                    // state, and retry the persist up to two more times. Other
                    // error variants fail immediately (same as before).
                    // The rows of the scheduled actions it arms are written
                    // with the event, so a restart arms them again.
                    let scheduled = self
                        .arm_scheduled_actions(
                            &mut result.scheduled_actions,
                            &result.custom_effects,
                            state,
                            &event,
                            caller.as_deref(),
                        )
                        .await;
                    // A machine's move that leaves its record as it was writes
                    // nothing (`unrecorded`); its triggers still run.
                    let fires_timer_once = super::timer_fire::fires_once(
                        caller.as_deref(),
                        &self.tenant,
                        state,
                        &name,
                    );
                    // Bringing a timer forward changes its due time through
                    // this event's status clock, even if no fields changed.
                    let brings_timer_forward = self
                        .table
                        .read()
                        .expect("transition table poisoned")
                        .action_contracts
                        .get(&name)
                        .is_some_and(|contract| contract.fire_now_in.contains(&state.status));
                    if !brings_timer_forward
                        && super::unrecorded::made_by_machine(&event.origin)
                        && super::unrecorded::leaves_record_unchanged(
                            &state_before,
                            state,
                            &result,
                            &event,
                            &scheduled,
                            fires_timer_once,
                        )
                    {
                        *state = state_before;
                        let response = self.answer_unrecorded(state, result, &event);
                        if let (Some(key), Some(request), Some(cache)) = (
                            idempotency_key.as_ref(),
                            request.as_ref(),
                            self.idempotency_cache.as_ref(),
                        ) {
                            cache.put(&actor_key, key, request, &response);
                        }
                        ctx.reply(response);
                        crate::runtime_metrics::record_actor_ask_reply_latency(
                            &state.entity_type,
                            &name,
                            ask_reply_start.elapsed(),
                        );
                        return Ok(());
                    }
                    if let (Some(store), Some(backend)) =
                        (self.event_journal.as_ref(), self.event_backend)
                    {
                        let first_persist = self
                            .persist_event(
                                store,
                                backend,
                                state,
                                &event,
                                &scheduled,
                                caller.as_deref(),
                            )
                            .await;

                        match first_persist {
                            Ok(_) => {
                                // Happy path — fall through to downstream telemetry.
                            }
                            Err(PersistenceError::ConcurrencyViolation {
                                expected: _,
                                actual,
                            }) => {
                                // ADR-0046 Sub-Decision 3: dedicated APM span
                                // covering the retry cycle. `attempts` and
                                // `outcome` are recorded at the end so Datadog
                                // APM can filter and chart conflict-handling
                                // activity per entity type.
                                let retry_span = tracing::info_span!(
                                    "temper.entity.persist_with_retry",
                                    "entity.type" = %self.entity_type,
                                    "entity.id" = %state.entity_id,
                                    action = %name,
                                    initial_actual = actual,
                                    attempts = tracing::field::Empty,
                                    outcome = tracing::field::Empty,
                                );

                                if timer_armed_seq.is_some_and(|armed| actual > armed) {
                                    // The replay below drops this fire.
                                    tracing::info!(
                                        parent: &retry_span,
                                        entity = %state.entity_id,
                                        action = %name,
                                        actual_seq = actual,
                                        "state timer fire found its record past its arming; replaying to drop it"
                                    );
                                } else {
                                    tracing::warn!(
                                        parent: &retry_span,
                                        entity = %state.entity_id,
                                        action = %name,
                                        actual_seq = actual,
                                        "persist hit optimistic-concurrency violation; entering ADR-0046 retry"
                                    );
                                }

                                // 2 retries + 1 initial = 3 total attempts (ADR-0046).
                                const MAX_RETRIES: u32 = 2;
                                let mut retry_idx: u32 = 0;
                                let mut retry_final: Option<(
                                    crate::runtime_metrics::ConcurrencyRetryOutcome,
                                    Option<RetryGaveUp>,
                                )> = None;
                                // The typed reason when the replayed action is refused.
                                let mut retry_refusal = None;
                                // ADR-0046 Sub-Decision 4: track the most
                                // recent authoritative sequence across retries
                                // so the post-replay assertion catches a
                                // divergent replay even on a multi-conflict
                                // cycle. Seeded from the initial violation;
                                // refreshed from each subsequent violation.
                                let mut last_actual: u64 = actual;

                                while retry_idx < MAX_RETRIES {
                                    retry_idx += 1;

                                    // Rollback speculative state.
                                    *state = state_before.clone();

                                    // Catch up to the authoritative sequence.
                                    self.now
                                        .during(
                                            Doing::Replaying,
                                            Self::replay_events(
                                                &table,
                                                store,
                                                backend,
                                                state,
                                                &self.tenant,
                                                self.blob_store.as_ref(),
                                                (
                                                    ReplayPolicy::StrictSnapshot,
                                                    ReplayLogging::Actor,
                                                ),
                                            ),
                                        )
                                        .await?;

                                    // ADR-0046 Sub-Decision 4: replay must at
                                    // minimum reach the sequence the store
                                    // reported. Reaching further is fine (a
                                    // later writer may have appended during
                                    // our own round trip).
                                    debug_assert!(
                                        state.sequence_nr >= last_actual,
                                        "POSTCONDITION: replay under-reached authoritative sequence \
                                         (state.sequence_nr={} < last_actual={last_actual})",
                                        state.sequence_nr
                                    );

                                    // Refresh baselines so postconditions hold
                                    // against the replayed state, not the
                                    // pre-race snapshot.
                                    state_before = state.clone();
                                    event_count_before = state.total_event_count;

                                    // This attempt's own write did not land
                                    // (`persist_event` read the journal). If the
                                    // journal holds this action's key, another
                                    // ask carrying the same key wrote it: writing
                                    // it again would apply it twice, and its
                                    // guard refusing it now is not a refusal.
                                    if let Some(key) = idempotency_key.as_deref()
                                        && state.has_processed_idempotency_key(key)
                                    {
                                        retry_final = Some((
                                            crate::runtime_metrics::ConcurrencyRetryOutcome::AlreadyWritten,
                                            Some(RetryGaveUp::WriteFailed(format!(
                                                "action {name} may have been written: the journal holds its key after a concurrency violation"
                                            ))),
                                        ));
                                        break;
                                    }

                                    // A timer's fire is not evaluated again on
                                    // a record that moved past its arming: the
                                    // move was another writer's fire of the same
                                    // arming, or a change the timer's next arm
                                    // follows. With its key absent from the
                                    // journal, this fire did not land.
                                    if let Some(error) =
                                        super::timer_fire::moved_past(timer_armed_seq, state)
                                    {
                                        let gave_up = if idempotency_key.is_some() {
                                            RetryGaveUp::Refused(error)
                                        } else {
                                            RetryGaveUp::WriteFailed(error)
                                        };
                                        retry_final = Some((
                                            crate::runtime_metrics::ConcurrencyRetryOutcome::TimerSuperseded,
                                            Some(gave_up),
                                        ));
                                        break;
                                    }

                                    // An action its caller decided on the state
                                    // it read (Cedar and the route's checks) is
                                    // not evaluated again on the caught-up one,
                                    // which they never saw. Its key is absent
                                    // from the journal, so it did not land: the
                                    // caller decides it again on this state.
                                    if expected_authorization_precondition.is_some() {
                                        retry_final = Some((
                                            crate::runtime_metrics::ConcurrencyRetryOutcome::RecordChanged,
                                            Some(RetryGaveUp::RecordChanged),
                                        ));
                                        break;
                                    }

                                    // Re-evaluate the action against the caught-up
                                    // state. It may now fail (entity reached a
                                    // terminal state during the race) — if so,
                                    // surface that error rather than silently
                                    // dropping the caller.
                                    let mut retry_result = self
                                        .now
                                        .during(
                                            Doing::ReadingBlobs,
                                            process_action_with_blob_prestate(
                                                state,
                                                &table,
                                                &name,
                                                &params,
                                                &related,
                                                field_sync_mode,
                                                crate::blobs::BlobReadSource::Staged {
                                                    store: self.blob_store.as_ref(),
                                                    legacy: self.legacy_blob_store.as_deref(),
                                                    blobs: &[],
                                                },
                                            ),
                                        )
                                        .await;

                                    if !retry_result.success {
                                        retry_refusal = retry_result.refusal;
                                        let error = retry_result.error.unwrap_or_else(|| {
                                            format!(
                                                "action {name} no longer legal after concurrency replay"
                                            )
                                        });
                                        // Without a key the journal cannot show
                                        // whether an earlier attempt landed.
                                        let gave_up = if idempotency_key.is_some() {
                                            RetryGaveUp::Refused(error)
                                        } else {
                                            RetryGaveUp::WriteFailed(error)
                                        };
                                        retry_final = Some((
                                            crate::runtime_metrics::ConcurrencyRetryOutcome::ActionIllegal,
                                            Some(gave_up),
                                        ));
                                        break;
                                    }

                                    let retry_event = retry_result
                                        .event
                                        .clone()
                                        .expect("successful process_action always returns event"); // ci-ok: post-assertion, success guarantees Some
                                    let mut retry_event = retry_event;
                                    retry_event.idempotency_key = idempotency_key.clone();
                                    retry_event.origin =
                                        super::EventOrigin::from_caller(caller.as_deref());

                                    // Overflow blobs for the re-evaluated result.
                                    if !retry_result.overflow_blobs.is_empty()
                                        && let Err(e) = self
                                            .now
                                            .during(
                                                Doing::WritingBlobs,
                                                Self::persist_overflow_blobs(
                                                    self.blob_store.as_ref(),
                                                    &retry_result.overflow_blobs,
                                                ),
                                            )
                                            .await
                                    {
                                        retry_final = Some((
                                            crate::runtime_metrics::ConcurrencyRetryOutcome::Exhausted,
                                            Some(e.into()),
                                        ));
                                        break;
                                    }

                                    // Backoff: retry 1 → 10ms, retry 2 → 50ms.
                                    let backoff_ms = if retry_idx == 1 { 10 } else { 50 };
                                    self.now
                                        .during(
                                            Doing::RetryPause,
                                            sleep_persistence_retry(
                                                std::time::Duration::from_millis(backoff_ms),
                                            ),
                                        )
                                        .await;

                                    let retry_scheduled = self
                                        .arm_scheduled_actions(
                                            &mut retry_result.scheduled_actions,
                                            &retry_result.custom_effects,
                                            state,
                                            &retry_event,
                                            caller.as_deref(),
                                        )
                                        .await;
                                    match self
                                        .persist_event(
                                            store,
                                            backend,
                                            state,
                                            &retry_event,
                                            &retry_scheduled,
                                            caller.as_deref(),
                                        )
                                        .await
                                    {
                                        Ok(_) => {
                                            // Commit re-evaluated event + result into
                                            // downstream telemetry and reply.
                                            event = retry_event;
                                            result = retry_result;
                                            retry_final = Some((
                                                crate::runtime_metrics::ConcurrencyRetryOutcome::Success,
                                                None,
                                            ));
                                            break;
                                        }
                                        Err(PersistenceError::ConcurrencyViolation {
                                            actual: new_actual,
                                            ..
                                        }) if retry_idx < MAX_RETRIES => {
                                            // Capture the fresh authoritative
                                            // sequence so the next iteration's
                                            // post-replay assertion checks
                                            // against the right target.
                                            last_actual = new_actual;
                                            tracing::warn!(
                                                parent: &retry_span,
                                                entity = %state.entity_id,
                                                action = %name,
                                                attempt = retry_idx + 1,
                                                actual_seq = new_actual,
                                                "retry persist hit another concurrency violation; retrying"
                                            );
                                            continue;
                                        }
                                        Err(PersistenceError::ConcurrencyViolation { .. }) => {
                                            // The last attempt did not land
                                            // either (`persist_event` read the
                                            // journal); the record kept moving.
                                            retry_final = Some((
                                                crate::runtime_metrics::ConcurrencyRetryOutcome::Exhausted,
                                                Some(RetryGaveUp::WriteFailed(
                                                    "persistence failed: optimistic concurrency retry exhausted"
                                                        .to_string(),
                                                )),
                                            ));
                                            break;
                                        }
                                        Err(e) => {
                                            retry_final = Some((
                                                crate::runtime_metrics::ConcurrencyRetryOutcome::Exhausted,
                                                Some(RetryGaveUp::NotTaken(e)),
                                            ));
                                            break;
                                        }
                                    }
                                }

                                // Record the retry outcome. `total_attempts` is
                                // 1-based; `retry_idx` counts completed retries.
                                let total_attempts = u64::from(1 + retry_idx);
                                if let Some((outcome, gave_up)) = retry_final {
                                    // Close the ADR-0046 APM span with the
                                    // final attempt count + outcome so APM
                                    // views can filter by either.
                                    retry_span.record("attempts", total_attempts);
                                    retry_span.record("outcome", outcome.as_str());
                                    crate::runtime_metrics::record_entity_concurrency_retry(
                                        &self.entity_type,
                                        outcome,
                                        total_attempts,
                                    );
                                    if let Some(gave_up) = gave_up {
                                        *state = state_before;
                                        let reply = match gave_up {
                                            RetryGaveUp::Refused(error) => EntityResponse {
                                                refusal: retry_refusal,
                                                ..EntityResponse::refused(state.clone(), error)
                                            },
                                            RetryGaveUp::WriteFailed(error) => EntityResponse {
                                                refusal: retry_refusal,
                                                ..EntityResponse::write_failed(state.clone(), error)
                                            },
                                            RetryGaveUp::RecordChanged => {
                                                EntityResponse::record_changed(
                                                    state.clone(),
                                                    ACTION_RECORD_CHANGED.to_string(),
                                                )
                                            }
                                            RetryGaveUp::NotTaken(error) => {
                                                persistence_failure_reply(state.clone(), &error)
                                            }
                                        };
                                        ctx.reply(reply);
                                        return Ok(());
                                    }
                                }
                            }
                            Err(e) => {
                                // Non-concurrency persistence error: roll back and fail.
                                *state = state_before;
                                ctx.reply(persistence_failure_reply(state.clone(), &e));
                                return Ok(());
                            }
                        }
                    }

                    // Telemetry as Views: emit wide event → OTEL span + metrics.
                    // Duration covers evaluate + effects + persist (the full
                    // actor-side work). DST-safe: sim_now() diff is 0 in
                    // simulation (same logical tick), real wall-clock in production.
                    let action_end = sim_now();
                    let duration_ns = (action_end - action_start)
                        .num_nanoseconds()
                        .unwrap_or(0)
                        .max(0) as u64;
                    let wide = wide_event::from_transition(wide_event::TransitionInput {
                        tenant: &self.tenant,
                        entity_type: &state.entity_type,
                        entity_id: &state.entity_id,
                        operation: &name,
                        from_status: &event.from_status,
                        to_status: &state.status,
                        success: true,
                        duration_ns,
                        params: &event.params,
                        item_count: state.item_count,
                        trace_id: &self.trace_id,
                    });
                    wide_event::emit_span(&wide);
                    wide_event::emit_metrics(&wide);

                    state.push_event_bounded(event);

                    let persistence_id = self.persistence_id();
                    if let Some(ref store) = self.event_journal
                        && let Err(e) = self
                            .now
                            .during(
                                Doing::Snapshotting,
                                Self::maybe_save_snapshot(
                                    store,
                                    self.snapshot_queue.as_ref(),
                                    &persistence_id,
                                    state,
                                ),
                            )
                            .await
                    {
                        tracing::warn!(
                            entity = %state.entity_id,
                            seq = state.sequence_nr,
                            error = %e,
                            "failed to persist snapshot"
                        );
                    }

                    // TigerStyle: Assert postconditions after every transition.
                    debug_assert!(
                        table.states.contains(&state.status),
                        "POSTCONDITION: status '{}' not in valid states after {}",
                        state.status,
                        name
                    );
                    debug_assert!(
                        state.total_event_count == event_count_before + 1,
                        "POSTCONDITION: event count must grow by exactly 1 (was {}, now {})",
                        event_count_before,
                        state.total_event_count
                    );
                    debug_assert!(
                        state
                            .events
                            .back()
                            .expect("events non-empty after push")
                            .action
                            == name, // ci-ok: post-assertion, just pushed an event
                        "POSTCONDITION: last event must be the action that just fired"
                    );

                    tracing::info!(
                        entity = %state.entity_id,
                        action = %name,
                        to = %state.status,
                        events_total = state.total_event_count,
                        events_since_snapshot = state.events_since_snapshot,
                        events_recent = state.events.len(),
                        "transition applied"
                    );

                    let response = EntityResponse {
                        success: true,
                        refusal: None,
                        state: state.clone(),
                        error: None,
                        custom_effects: result.custom_effects,
                        scheduled_actions: result.scheduled_actions,
                        spawn_requests: result.spawn_requests,
                        spec_governed: true,
                        refused: false,
                        answered_action: None,
                        trigger_failures: Vec::new(),
                        store_unavailable: false,
                        unrecorded_at: None,
                    };
                    // ADR-0048 sub-decision 5: cache the successful response
                    // so a racing retry that lands after this reply returns
                    // the cached value instead of re-executing.
                    if let (Some(key), Some(request), Some(cache)) = (
                        idempotency_key.as_ref(),
                        request.as_ref(),
                        self.idempotency_cache.as_ref(),
                    ) {
                        cache.put(&actor_key, key, request, &response);
                    }
                    ctx.reply(response);
                } else {
                    // Transition failed — emit telemetry
                    let action_end = sim_now();
                    let duration_ns = (action_end - action_start)
                        .num_nanoseconds()
                        .unwrap_or(0)
                        .max(0) as u64;
                    let wide = wide_event::from_transition(wide_event::TransitionInput {
                        tenant: &self.tenant,
                        entity_type: &state.entity_type,
                        entity_id: &state.entity_id,
                        operation: &name,
                        from_status: &state.status,
                        to_status: &state.status,
                        success: false,
                        duration_ns,
                        params: &params,
                        item_count: state.item_count,
                        trace_id: &self.trace_id,
                    });
                    wide_event::emit_span(&wide);
                    wide_event::emit_metrics(&wide);

                    ctx.reply(EntityResponse {
                        refusal: result.refusal,
                        ..EntityResponse::refused(
                            state.clone(),
                            result
                                .error
                                .unwrap_or_else(|| format!("action {name} refused")),
                        )
                    });
                }
                // Inside-actor ask reply latency (excludes dispatch and retry
                // overhead). Early-exit error paths above `return Ok(())` are
                // not counted; the signal of interest is normal action
                // handling latency.
                crate::runtime_metrics::record_actor_ask_reply_latency(
                    &state.entity_type,
                    &name,
                    ask_reply_start.elapsed(),
                );
            }
            EntityMsg::GetState => {
                ctx.reply(EntityResponse {
                    success: true,
                    refusal: None,
                    state: state.clone(),
                    error: None,
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                    refused: false,
                    answered_action: None,
                    trigger_failures: Vec::new(),
                    store_unavailable: false,
                    unrecorded_at: None,
                });
            }
            EntityMsg::UpdateFields {
                caller,
                fields,
                replace,
                expected_precondition,
            } => {
                // Refused before anything is written, like a stale action or
                // delete, so the route decides the update again on this state.
                if let Some(expected) = expected_precondition.as_deref()
                    && super::effects::entity_authorization_precondition(state) != expected
                {
                    ctx.reply(EntityResponse::record_changed(
                        state.clone(),
                        super::field_updates::STALE_AUTHORIZATION.to_string(),
                    ));
                    return Ok(());
                }
                // The whole durable transaction — validation, budget, journal
                // append, conflict recovery — lives in `field_updates`. This arm
                // only turns its outcome into a reply.
                let outcome = super::field_updates::commit_field_update(
                    self,
                    state,
                    fields,
                    replace,
                    expected_precondition.is_some(),
                    caller.as_deref(),
                )
                .await;
                ctx.reply(match outcome {
                    Ok(()) => EntityResponse {
                        success: true,
                        refusal: None,
                        state: state.clone(),
                        error: None,
                        custom_effects: vec![],
                        scheduled_actions: vec![],
                        spawn_requests: vec![],
                        spec_governed: true,
                        refused: false,
                        answered_action: None,
                        trigger_failures: Vec::new(),
                        store_unavailable: false,
                        unrecorded_at: None,
                    },
                    Err(NotUpdated::RecordChanged) => EntityResponse::record_changed(
                        state.clone(),
                        super::field_updates::STALE_AUTHORIZATION.to_string(),
                    ),
                    Err(NotUpdated::Failed(reason)) => {
                        EntityResponse::write_failed(state.clone(), reason)
                    }
                    Err(NotUpdated::Store(error)) => {
                        persistence_failure_reply(state.clone(), &error)
                    }
                });
            }

            EntityMsg::RecordSpawnRefusal { refusal } => {
                let outcome = super::spawn_refusal::commit(self, state, &refusal).await;
                ctx.reply(EntityResponse {
                    success: outcome.is_ok(),
                    refusal: None,
                    state: state.clone(),
                    error: outcome.err(),
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                    refused: false,
                    answered_action: None,
                    trigger_failures: Vec::new(),
                    store_unavailable: false,
                    unrecorded_at: None,
                });
            }

            EntityMsg::RecordTimerNeedsPerson { action, checked } => {
                let outcome =
                    super::timer_needs_person::commit(self, state, &action, checked).await;
                // A record that moved since is refused, with nothing written.
                let moved = outcome == Ok(super::timer_needs_person::Stop::Moved);
                ctx.reply(EntityResponse {
                    success: outcome.is_ok() && !moved,
                    refusal: None,
                    state: state.clone(),
                    error: outcome.err(),
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                    refused: moved,
                    answered_action: None,
                    trigger_failures: Vec::new(),
                    store_unavailable: false,
                    unrecorded_at: None,
                });
            }

            EntityMsg::Delete {
                caller,
                expected_authorization_precondition,
            } => {
                if self
                    .table
                    .read()
                    .expect("table lock poisoned")
                    .strict_action_params
                {
                    ctx.reply(EntityResponse::refused(
                        state.clone(),
                        "Strict entities require a declared action for deletion".to_owned(),
                    ));
                    return Ok(());
                }
                let has_precondition = expected_authorization_precondition.is_some();
                if let Some(expected) = expected_authorization_precondition
                    && super::effects::entity_authorization_precondition(state) != expected
                {
                    ctx.reply(EntityResponse::record_changed(
                        state.clone(),
                        STALE_DELETE.to_string(),
                    ));
                    return Ok(());
                }
                // Deleted already, by a delete that reached it first: the
                // record is what this one asked for, and a second tombstone
                // would record nothing. A tombstone is terminal, so it is the
                // record's newest event; a spec's own `Deleted` state is not
                // one, and still takes a tombstone.
                if state.events.back().is_some_and(|event| {
                    event.kind == temper_runtime::persistence::EventKind::Tombstone
                }) {
                    ctx.reply(EntityResponse {
                        success: true,
                        state: state.clone(),
                        error: None,
                        refusal: None,
                        custom_effects: vec![],
                        scheduled_actions: vec![],
                        spawn_requests: vec![],
                        spec_governed: true,
                        refused: false,
                        answered_action: None,
                        trigger_failures: Vec::new(),
                        store_unavailable: false,
                        unrecorded_at: None,
                    });
                    return Ok(());
                }
                let mut deleted = EntityEvent::tombstone(state.status.clone());
                deleted.origin = super::EventOrigin::from_caller(caller.as_deref());

                if let (Some(store), Some(backend)) =
                    (self.event_journal.as_ref(), self.event_backend)
                    && let Err(e) = self
                        .persist_event(
                            store,
                            backend,
                            state,
                            &deleted,
                            &ScheduledRows::default(),
                            None,
                        )
                        .await
                {
                    // A delete decided on a state the journal had left, as an
                    // action or a field update is: caught up, it is refused
                    // as RecordChanged, and the route decides it again.
                    let reply = match e {
                        PersistenceError::ConcurrencyViolation { actual, .. }
                            if has_precondition =>
                        {
                            match super::field_updates::catch_up(self, state, 1, actual, "Delete")
                                .await
                            {
                                Ok(()) => EntityResponse::record_changed(
                                    state.clone(),
                                    STALE_DELETE.to_string(),
                                ),
                                Err(reason) => EntityResponse::write_failed(state.clone(), reason),
                            }
                        }
                        e => persistence_failure_reply(state.clone(), &e),
                    };
                    ctx.reply(reply);
                    return Ok(());
                }

                state.record_tombstone(deleted);

                ctx.reply(EntityResponse {
                    success: true,
                    refusal: None,
                    state: state.clone(),
                    error: None,
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                    refused: false,
                    answered_action: None,
                    trigger_failures: Vec::new(),
                    store_unavailable: false,
                    unrecorded_at: None,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "actor_test.rs"]
mod tests;

#[cfg(test)]
#[path = "authoritative_replay_test.rs"]
mod authoritative_replay_tests;

#[cfg(all(test, feature = "sim"))]
#[path = "event_size_test.rs"]
mod event_size_tests;

#[cfg(test)]
#[path = "contract_state_test.rs"]
pub(super) mod contract_state_tests;

#[cfg(all(test, feature = "sim"))]
#[path = "held_action_test.rs"]
mod held_action_tests;

#[cfg(all(test, feature = "sim"))]
#[path = "hydration_test.rs"]
mod hydration_tests;

#[cfg(all(test, feature = "sim"))]
#[path = "blob_refusal_test.rs"]
mod blob_refusal_tests;

#[cfg(all(test, feature = "sim"))]
#[path = "record_changed_test.rs"]
mod record_changed_tests;
