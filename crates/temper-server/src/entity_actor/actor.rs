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
    COMPOSITE_EVENT_TYPE, CompositeEvent, EventMetadata, IndexReconciliation, JournalBoundary,
    PersistenceEnvelope, PersistenceError, SnapshotSourceFence, is_state_materialization_event_for,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};

use crate::storage::{BackendLabel, BoxedEventStore};

use super::effects::{
    FieldSyncMode, ProcessResult, ScheduledAction, SpawnRequest, build_eval_context_with_xref,
    process_action_with_xref_and_field_mode, prune_transient_action_fields_from_state,
};
use super::snapshot_queue::{SnapshotEnqueueOutcome, SnapshotWriteQueue};
use super::types::{
    EntityEvent, EntityMsg, EntityResponse, EntityState, MAX_EVENTS_SINCE_SNAPSHOT,
    MAX_ITEMS_PER_ENTITY,
};

mod field_update_idempotency;
mod field_updates;
mod persistence;
mod recovery;
mod state_materialization;

#[cfg(test)]
pub(crate) use recovery::recover_entity_state_from_store;
pub(crate) use recovery::{
    CapturedEntitySnapshot, EntityRecoveryContext, StableEntitySource,
    recover_entity_state_from_stable_sources, recover_entity_state_with_source_from_store,
    stable_entity_source_is_current,
};

use field_update_idempotency::field_update_intent_fingerprint;
use field_updates::{
    FIELD_UPDATE_EVENT_TYPE, FIELD_UPDATE_SCHEMA, FIELDS_PATCHED_EVENT_TYPE,
    FIELDS_REPLACED_EVENT_TYPE, PersistedFieldUpdate,
};
#[cfg(test)]
pub(crate) use state_materialization::STATE_MATERIALIZATION_EVENT_TYPE;
use state_materialization::rebase_materialized_idempotency_keys;
pub(crate) use state_materialization::{
    PersistedStateMaterialization, STATE_MATERIALIZATION_SCHEMA, state_materialization_envelope,
};

const SNAPSHOT_JOURNAL_SEQUENCE_FIELD: &str = "_temper_snapshot_journal_sequence";
const POST_DISPATCH_EFFECTS_EVENT_TYPE: &str = "Temper.Internal.PostDispatchEffects.v1";
const POST_DISPATCH_EFFECTS_SCHEMA: &str = "temper.post-dispatch-effects.v1";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedPostDispatchEffects {
    schema: String,
    idempotency_key: String,
    custom_effects: Vec<String>,
    scheduled_actions: Vec<ScheduledAction>,
    spawn_requests: Vec<SpawnRequest>,
    source_fields: serde_json::Value,
    source_status: String,
    source_sequence: u64,
}

impl PersistedPostDispatchEffects {
    fn from_result(
        idempotency_key: Option<&str>,
        state: &EntityState,
        result: &ProcessResult,
    ) -> Option<Self> {
        let idempotency_key = idempotency_key.filter(|key| !key.is_empty())?;
        if result.custom_effects.is_empty()
            && result.scheduled_actions.is_empty()
            && result.spawn_requests.is_empty()
        {
            return None;
        }
        Some(Self {
            schema: POST_DISPATCH_EFFECTS_SCHEMA.to_string(),
            idempotency_key: idempotency_key.to_string(),
            custom_effects: result.custom_effects.clone(),
            scheduled_actions: result.scheduled_actions.clone(),
            spawn_requests: result.spawn_requests.clone(),
            source_fields: state.fields.clone(),
            source_status: state.status.clone(),
            source_sequence: 0,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotProvenance {
    Legacy,
    Journal { through_sequence: u64 },
}

struct PersistencePayload<'a> {
    event_type: &'a str,
    payload: serde_json::Value,
    timestamp: chrono::DateTime<chrono::Utc>,
    to_status: &'a str,
    post_dispatch_effects: Option<PersistedPostDispatchEffects>,
}

fn durable_conflict_sequence(error: &PersistenceError, snapshot_fallback: u64) -> Option<u64> {
    match error {
        PersistenceError::ConcurrencyViolation { actual, .. } => Some(*actual),
        PersistenceError::SnapshotGenerationChanged => Some(snapshot_fallback),
        _ => None,
    }
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

fn fallback_duplicate_idempotency_response(
    table: &TransitionTable,
    state: &EntityState,
    action: &str,
    cross_entity_booleans: &BTreeMap<String, bool>,
) -> EntityResponse {
    let custom_effects =
        duplicate_idempotency_custom_effects(table, state, action, cross_entity_booleans);
    let mut response_state = state.clone();
    if !custom_effects.is_empty() {
        prune_transient_action_fields_from_state(&mut response_state);
    }
    EntityResponse {
        success: true,
        state: response_state,
        error: None,
        custom_effects,
        scheduled_actions: vec![],
        spawn_requests: vec![],
        spec_governed: true,
    }
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
    /// Exact snapshot generation that produced the actor's current durable state.
    snapshot_source: Arc<RwLock<SnapshotSourceFence>>,
    /// Exact declared-key activation contract that produced the actor state.
    ///
    /// This is actor-state provenance, not a live-table lookup: the shared table
    /// may hot-swap before an old actor is evicted during a contract cutover.
    state_key_contract: Arc<RwLock<String>>,
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
    async fn duplicate_idempotency_response(
        &self,
        table: &TransitionTable,
        state: &EntityState,
        action: &str,
        cross_entity_booleans: &BTreeMap<String, bool>,
        idempotency_key: &str,
    ) -> Result<EntityResponse, ActorError> {
        let Some(sequence) = state
            .processed_idempotency_keys
            .get(idempotency_key)
            .copied()
        else {
            return Ok(fallback_duplicate_idempotency_response(
                table,
                state,
                action,
                cross_entity_booleans,
            ));
        };
        let Some(store) = self.event_journal.as_ref() else {
            return Ok(fallback_duplicate_idempotency_response(
                table,
                state,
                action,
                cross_entity_booleans,
            ));
        };
        let envelopes = store
            .read_events_page(
                &self.persistence_id(),
                sequence.saturating_sub(1),
                sequence,
                1,
            )
            .await
            .map_err(|error| {
                ActorError::custom(format!(
                    "failed to reload durable post-dispatch effects for {}:{} idempotency key '{}': {error}",
                    self.entity_type, self.entity_id, idempotency_key
                ))
            })?;
        let Some(envelope) = envelopes.first() else {
            return Err(ActorError::custom(format!(
                "durable idempotency sequence {sequence} is missing for {}:{} key '{}'",
                self.entity_type, self.entity_id, idempotency_key
            )));
        };
        if envelope.sequence_nr != sequence {
            return Err(ActorError::custom(format!(
                "durable idempotency sequence mismatch for {}:{} key '{}' (expected {sequence}, found {})",
                self.entity_type, self.entity_id, idempotency_key, envelope.sequence_nr
            )));
        }
        if envelope.event_type != POST_DISPATCH_EFFECTS_EVENT_TYPE {
            return Ok(fallback_duplicate_idempotency_response(
                table,
                state,
                action,
                cross_entity_booleans,
            ));
        }
        let persisted = serde_json::from_value::<PersistedPostDispatchEffects>(
            envelope.payload.clone(),
        )
        .map_err(|error| {
            ActorError::custom(format!(
                "failed to decode durable post-dispatch effects for {}:{} at sequence {sequence}: {error}",
                self.entity_type, self.entity_id
            ))
        })?;
        if persisted.schema != POST_DISPATCH_EFFECTS_SCHEMA
            || persisted.idempotency_key != idempotency_key
            || persisted.source_sequence != sequence
        {
            return Err(ActorError::custom(format!(
                "durable post-dispatch effect identity mismatch for {}:{} key '{}' at sequence {sequence}",
                self.entity_type, self.entity_id, idempotency_key
            )));
        }
        let mut response_state = state.clone();
        response_state.fields = persisted.source_fields;
        response_state.status = persisted.source_status;
        response_state.sequence_nr = persisted.source_sequence;
        Ok(EntityResponse {
            success: true,
            state: response_state,
            error: None,
            custom_effects: persisted.custom_effects,
            scheduled_actions: persisted.scheduled_actions,
            spawn_requests: persisted.spawn_requests,
            spec_governed: true,
        })
    }

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
            snapshot_source: Arc::new(RwLock::new(SnapshotSourceFence::Unchecked)),
            state_key_contract: Arc::new(RwLock::new(String::new())),
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
            snapshot_source: Arc::new(RwLock::new(SnapshotSourceFence::Unchecked)),
            state_key_contract: Arc::new(RwLock::new(String::new())),
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

impl Actor for EntityActor {
    type Msg = EntityMsg;
    type State = EntityState;

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<Self::State, ActorError> {
        // Snapshot the table for consistent startup (initial state + replay).
        // This is a cheap clone — TransitionTable is a few Vecs of strings.
        let table = self.table.read().expect("table lock poisoned").clone();
        self.record_state_key_contract(&table);

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
            let recovered = recover_entity_state_with_source_from_store(
                EntityRecoveryContext {
                    tenant: &self.tenant,
                    entity_type: &self.entity_type,
                    entity_id: &self.entity_id,
                    table: &table,
                    store,
                    backend,
                    initial_fields: &self.initial_fields,
                    blob_store: self.blob_store.as_ref(),
                },
                false, // empty captured journals may start fresh; any non-empty or unstable source fails closed
            )
            .await?;
            *self
                .snapshot_source
                .write()
                .expect("snapshot source lock poisoned") = recovered.snapshot_source;
            state = recovered.state;
        }

        // Persist a bootstrap Created event for first-time entities so initial
        // fields are durable and replayable. Snapshot-only migration baselines
        // use the same first-journal sequence and exact source fence.
        if let (Some(store), Some(backend)) = (self.event_journal.as_ref(), self.event_backend) {
            const MAX_BOOTSTRAP_RETRIES: u32 = 2;
            let mut retries = 0_u32;
            while state.sequence_nr == 0
                && state.total_event_count == 0
                && !matches!(
                    &*self
                        .snapshot_source
                        .read()
                        .expect("snapshot source lock poisoned"),
                    SnapshotSourceFence::Exact { .. }
                )
            {
                let state_before_created = state.clone();
                let created = EntityEvent {
                    action: "Created".to_string(),
                    from_status: String::new(),
                    to_status: state.status.clone(),
                    timestamp: sim_now(),
                    params: self.initial_fields.clone(),
                    idempotency_key: None,
                };
                match self
                    .persist_event(
                        store,
                        backend,
                        &self.persistence_id(),
                        &table,
                        &state_before_created,
                        &mut state,
                        &created,
                        None,
                    )
                    .await
                {
                    Ok(_) => {
                        state.push_event_bounded(created);
                        break;
                    }
                    Err(error)
                        if durable_conflict_sequence(&error, state.sequence_nr).is_some()
                            && retries < MAX_BOOTSTRAP_RETRIES =>
                    {
                        state = self
                            .recover_authoritative_state(store, backend, &table)
                            .await?;
                        retries += 1;
                    }
                    Err(error) => {
                        return Err(ActorError::custom(format!(
                            "failed to persist bootstrap Created event for {}:{}: {error}",
                            self.entity_type, self.entity_id
                        )));
                    }
                }
            }
        }

        Ok(state)
    }

    async fn handle(
        &self,
        msg: Self::Msg,
        state: &mut Self::State,
        ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        match msg {
            EntityMsg::Action {
                name,
                params,
                cross_entity_booleans,
                idempotency_key,
            } => {
                // Capture start time for span duration (DST-safe: sim_now()
                // returns logical clock in simulation, wall clock in production).
                let action_start = sim_now();
                // Wall-clock start for `temper_actor_ask_reply_latency_ms`.
                // Separate from `action_start` because metrics emission is
                // outside the DST boundary; using Instant here is safe.
                let ask_reply_start = Instant::now(); // determinism-ok: observability only

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
                let actor_key = self.persistence_id();
                if let (Some(key), Some(cache)) =
                    (idempotency_key.as_ref(), self.idempotency_cache.as_ref())
                    && let Some(cached) = cache.get(&actor_key, key)
                {
                    ctx.reply(cached);
                    return Ok(());
                }
                if let Some(key) = idempotency_key.as_deref()
                    && state.has_processed_idempotency_key(key)
                {
                    ctx.reply(
                        self.duplicate_idempotency_response(
                            &table,
                            state,
                            &name,
                            &cross_entity_booleans,
                            key,
                        )
                        .await?,
                    );
                    return Ok(());
                }

                // TigerStyle: Assert preconditions before every transition.
                // These run in production, not just tests.
                debug_assert!(
                    table.states.contains(&state.status),
                    "PRECONDITION: status '{}' not in valid states {:?}",
                    state.status,
                    table.states
                );
                debug_assert!(
                    state.events_since_snapshot < MAX_EVENTS_SINCE_SNAPSHOT,
                    "PRECONDITION: event budget exhausted ({} >= {})",
                    state.events_since_snapshot,
                    MAX_EVENTS_SINCE_SNAPSHOT
                );
                debug_assert!(
                    state.item_count <= MAX_ITEMS_PER_ENTITY,
                    "PRECONDITION: item budget exceeded ({} > {})",
                    state.item_count,
                    MAX_ITEMS_PER_ENTITY
                );

                // TigerStyle: Budget enforcement (not just assertions -- hard limits)
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
                    ctx.reply(EntityResponse {
                        success: false,
                        state: state.clone(),
                        error: Some(format!(
                            "Event budget exhausted ({MAX_EVENTS_SINCE_SNAPSHOT} max since snapshot)"
                        )),
                        custom_effects: vec![],
                        scheduled_actions: vec![],
                        spawn_requests: vec![],
                        spec_governed: true,
                    });
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
                let mut result = process_action_with_xref_and_field_mode(
                    state,
                    &table,
                    &name,
                    &params,
                    &cross_entity_booleans,
                    field_sync_mode,
                );

                if result.success {
                    // process_action returned a successful transition with event.
                    // Clone out so `result.event` stays populated for re-use if
                    // the retry path needs to re-emit (simplifies lifetime here).
                    let mut event = result
                        .event
                        .clone()
                        .expect("successful process_action always returns event"); // ci-ok: post-assertion, success guarantees Some
                    event.idempotency_key = idempotency_key.clone();

                    if !result.overflow_blobs.is_empty()
                        && let Err(e) = Self::persist_overflow_blobs(
                            self.blob_store.as_ref(),
                            &result.overflow_blobs,
                        )
                        .await
                    {
                        *state = state_before;
                        ctx.reply(EntityResponse {
                            success: false,
                            state: state.clone(),
                            error: Some(format!("field-overflow blob persistence failed: {e}")),
                            custom_effects: vec![],
                            scheduled_actions: vec![],
                            spawn_requests: vec![],
                            spec_governed: true,
                        });
                        return Ok(());
                    }

                    // Persist to Postgres (if configured). On
                    // `ConcurrencyViolation` enter the ADR-0046 retry cycle —
                    // replay events, re-evaluate the action against the caught-up
                    // state, and retry the persist up to two more times. Other
                    // error variants fail immediately (same as before).
                    if let (Some(store), Some(backend)) =
                        (self.event_journal.as_ref(), self.event_backend)
                    {
                        let post_dispatch_effects = PersistedPostDispatchEffects::from_result(
                            idempotency_key.as_deref(),
                            state,
                            &result,
                        );
                        let first_persist = self
                            .persist_event(
                                store,
                                backend,
                                &self.persistence_id(),
                                &table,
                                &state_before,
                                state,
                                &event,
                                post_dispatch_effects,
                            )
                            .await;

                        match first_persist {
                            Ok(_) => {
                                // Happy path — fall through to downstream telemetry.
                            }
                            Err(error)
                                if durable_conflict_sequence(&error, state.sequence_nr)
                                    .is_some() =>
                            {
                                let actual = durable_conflict_sequence(&error, state.sequence_nr)
                                    .expect("guard accepted durable conflict");
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

                                tracing::warn!(
                                    parent: &retry_span,
                                    entity = %state.entity_id,
                                    action = %name,
                                    actual_seq = actual,
                                    "persist hit optimistic-concurrency violation; entering ADR-0046 retry"
                                );

                                // 2 retries + 1 initial = 3 total attempts (ADR-0046).
                                const MAX_RETRIES: u32 = 2;
                                let mut retry_idx: u32 = 0;
                                let mut retry_final: Option<(
                                    crate::runtime_metrics::ConcurrencyRetryOutcome,
                                    Option<String>,
                                )> = None;
                                // ADR-0046 Sub-Decision 4: track the most
                                // recent authoritative sequence across retries
                                // so the post-replay assertion catches a
                                // divergent replay even on a multi-conflict
                                // cycle. Seeded from the initial violation;
                                // refreshed from each subsequent violation.
                                let mut last_actual: u64 = actual;

                                while retry_idx < MAX_RETRIES {
                                    retry_idx += 1;

                                    // Catch up from a clean initial state. Replaying
                                    // onto the actor's pre-race state would apply
                                    // non-idempotent effects a second time.
                                    *state = self
                                        .recover_authoritative_state(store, backend, &table)
                                        .await
                                        .map_err(|error| {
                                            ActorError::custom(format!(
                                                "failed to catch up {}:{} after action concurrency loss: {error}",
                                                self.entity_type, self.entity_id
                                            ))
                                        })?;

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
                                    if state.sequence_nr < last_actual {
                                        return Err(ActorError::custom(format!(
                                            "action catch-up under-reached authoritative sequence for {}:{} ({} < {last_actual})",
                                            self.entity_type, self.entity_id, state.sequence_nr
                                        )));
                                    }

                                    // A competing writer may have committed this
                                    // exact idempotency key while our stale actor
                                    // was attempting the same action. Durable
                                    // replay is the authority: return the already
                                    // committed result instead of re-evaluating and
                                    // appending the action a second time.
                                    if let Some(key) = idempotency_key.as_deref()
                                        && state.has_processed_idempotency_key(key)
                                    {
                                        let total_attempts = u64::from(1 + retry_idx);
                                        let outcome = crate::runtime_metrics::ConcurrencyRetryOutcome::Success;
                                        retry_span.record("attempts", total_attempts);
                                        retry_span.record("outcome", outcome.as_str());
                                        crate::runtime_metrics::record_entity_concurrency_retry(
                                            &self.entity_type,
                                            outcome,
                                            total_attempts,
                                        );
                                        let response = self
                                            .duplicate_idempotency_response(
                                                &table,
                                                state,
                                                &name,
                                                &cross_entity_booleans,
                                                key,
                                            )
                                            .await?;
                                        if let Some(cache) = self.idempotency_cache.as_ref() {
                                            cache.put(&actor_key, key, response.clone());
                                        }
                                        ctx.reply(response);
                                        return Ok(());
                                    }

                                    // Refresh baselines so postconditions hold
                                    // against the replayed state, not the
                                    // pre-race snapshot.
                                    state_before = state.clone();
                                    event_count_before = state.total_event_count;

                                    // Re-evaluate the action against the caught-up
                                    // state. It may now fail (entity reached a
                                    // terminal state during the race) — if so,
                                    // surface that error rather than silently
                                    // dropping the caller.
                                    let retry_result = process_action_with_xref_and_field_mode(
                                        state,
                                        &table,
                                        &name,
                                        &params,
                                        &cross_entity_booleans,
                                        field_sync_mode,
                                    );

                                    if !retry_result.success {
                                        retry_final = Some((
                                            crate::runtime_metrics::ConcurrencyRetryOutcome::ActionIllegal,
                                            Some(retry_result.error.unwrap_or_else(|| {
                                                format!(
                                                    "action {name} no longer legal after concurrency replay"
                                                )
                                            })),
                                        ));
                                        break;
                                    }

                                    let retry_event = retry_result
                                        .event
                                        .clone()
                                        .expect("successful process_action always returns event"); // ci-ok: post-assertion, success guarantees Some
                                    let mut retry_event = retry_event;
                                    retry_event.idempotency_key = idempotency_key.clone();

                                    // Overflow blobs for the re-evaluated result.
                                    if !retry_result.overflow_blobs.is_empty()
                                        && let Err(e) = Self::persist_overflow_blobs(
                                            self.blob_store.as_ref(),
                                            &retry_result.overflow_blobs,
                                        )
                                        .await
                                    {
                                        retry_final = Some((
                                            crate::runtime_metrics::ConcurrencyRetryOutcome::Exhausted,
                                            Some(format!(
                                                "field-overflow blob persistence failed during retry: {e}"
                                            )),
                                        ));
                                        break;
                                    }

                                    // Backoff: retry 1 → 10ms, retry 2 → 50ms.
                                    let backoff_ms = if retry_idx == 1 { 10 } else { 50 };
                                    tokio::time::sleep(std::time::Duration::from_millis(
                                        backoff_ms,
                                    ))
                                    .await; // determinism-ok: rare retry backoff (ADR-0046)

                                    let post_dispatch_effects =
                                        PersistedPostDispatchEffects::from_result(
                                            idempotency_key.as_deref(),
                                            state,
                                            &retry_result,
                                        );

                                    match self
                                        .persist_event(
                                            store,
                                            backend,
                                            &self.persistence_id(),
                                            &table,
                                            &state_before,
                                            state,
                                            &retry_event,
                                            post_dispatch_effects,
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
                                        Err(error)
                                            if retry_idx < MAX_RETRIES
                                                && durable_conflict_sequence(
                                                    &error,
                                                    state.sequence_nr,
                                                )
                                                .is_some() =>
                                        {
                                            let new_actual = durable_conflict_sequence(
                                                &error,
                                                state.sequence_nr,
                                            )
                                            .expect("guard accepted durable conflict");
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
                                        Err(error)
                                            if durable_conflict_sequence(
                                                &error,
                                                state.sequence_nr,
                                            )
                                            .is_some() =>
                                        {
                                            retry_final = Some((
                                                crate::runtime_metrics::ConcurrencyRetryOutcome::Exhausted,
                                                Some(
                                                    "persistence failed: optimistic concurrency retry exhausted"
                                                        .to_string(),
                                                ),
                                            ));
                                            break;
                                        }
                                        Err(e) => {
                                            retry_final = Some((
                                                crate::runtime_metrics::ConcurrencyRetryOutcome::Exhausted,
                                                Some(format!(
                                                    "persistence failed during retry: {e}"
                                                )),
                                            ));
                                            break;
                                        }
                                    }
                                }

                                // Record the retry outcome. `total_attempts` is
                                // 1-based; `retry_idx` counts completed retries.
                                let total_attempts = u64::from(1 + retry_idx);
                                if let Some((outcome, err_msg)) = retry_final {
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
                                    if let Some(msg) = err_msg {
                                        *state = state_before;
                                        ctx.reply(EntityResponse {
                                            success: false,
                                            state: state.clone(),
                                            error: Some(msg),
                                            custom_effects: vec![],
                                            scheduled_actions: vec![],
                                            spawn_requests: vec![],
                                            spec_governed: true,
                                        });
                                        return Ok(());
                                    }
                                }
                            }
                            Err(e) => {
                                // Non-concurrency persistence error — unchanged:
                                // roll back and fail.
                                *state = state_before;
                                ctx.reply(EntityResponse {
                                    success: false,
                                    state: state.clone(),
                                    error: Some(format!("persistence failed: {e}")),
                                    custom_effects: vec![],
                                    scheduled_actions: vec![],
                                    spawn_requests: vec![],
                                    spec_governed: true,
                                });
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

                    let committed_idempotency_key = event.idempotency_key.clone();
                    state.push_event_bounded(event);
                    if self.event_journal.is_some()
                        && let Some(idempotency_key) = committed_idempotency_key.as_deref()
                    {
                        state.record_durable_idempotency_key(idempotency_key, state.sequence_nr);
                    }
                    self.record_state_key_contract(&table);

                    let persistence_id = self.persistence_id();
                    if let Some(ref store) = self.event_journal {
                        let mut snapshot_source = self
                            .snapshot_source
                            .read()
                            .expect("snapshot source lock poisoned")
                            .clone();
                        let key_contract = crate::key_index::declared_key_write_contract(&table);
                        match Self::maybe_save_snapshot(
                            store,
                            self.snapshot_queue.as_ref(),
                            &persistence_id,
                            state,
                            &mut snapshot_source,
                            Some(&key_contract),
                        )
                        .await
                        {
                            Ok(_) => {
                                *self
                                    .snapshot_source
                                    .write()
                                    .expect("snapshot source lock poisoned") = snapshot_source;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    entity = %state.entity_id,
                                    seq = state.sequence_nr,
                                    error = %e,
                                    "failed to persist snapshot"
                                );
                            }
                        }
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
                        state: state.clone(),
                        error: None,
                        custom_effects: result.custom_effects,
                        scheduled_actions: result.scheduled_actions,
                        spawn_requests: result.spawn_requests,
                        spec_governed: true,
                    };
                    // ADR-0048 sub-decision 5: cache the successful response
                    // so a racing retry that lands after this reply returns
                    // the cached value instead of re-executing.
                    if let (Some(key), Some(cache)) =
                        (idempotency_key.as_ref(), self.idempotency_cache.as_ref())
                    {
                        cache.put(&actor_key, key, response.clone());
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
                        success: false,
                        state: state.clone(),
                        error: result.error,
                        custom_effects: vec![],
                        scheduled_actions: vec![],
                        spawn_requests: vec![],
                        spec_governed: true,
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
                    state: state.clone(),
                    error: None,
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                });
            }
            EntityMsg::GetPassivationSnapshot => {
                ctx.reply(super::types::EntityPassivationSnapshot {
                    state: state.clone(),
                    snapshot_source: self
                        .snapshot_source
                        .read()
                        .expect("snapshot source lock poisoned")
                        .clone(),
                    key_contract: self
                        .state_key_contract
                        .read()
                        .expect("state key contract lock poisoned")
                        .clone(),
                });
            }
            EntityMsg::GetField { field } => {
                let value = state
                    .fields
                    .get(&field)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                ctx.reply(value);
            }
            EntityMsg::UpdateFields {
                fields,
                replace,
                idempotency_key,
            } => {
                self.handle_field_update(state, fields, replace, idempotency_key, ctx)
                    .await?;
            }
            EntityMsg::Delete => {
                const MAX_DELETE_RETRIES: u32 = 2;
                let mut retries = 0_u32;
                let table = self.table.read().expect("table lock poisoned").clone();
                let deleted = loop {
                    if state.status == "Deleted" {
                        ctx.reply(EntityResponse {
                            success: true,
                            state: state.clone(),
                            error: None,
                            custom_effects: vec![],
                            scheduled_actions: vec![],
                            spawn_requests: vec![],
                            spec_governed: true,
                        });
                        return Ok(());
                    }
                    if !state.can_accept_event() {
                        ctx.reply(EntityResponse {
                            success: false,
                            state: state.clone(),
                            error: Some(format!(
                                "Event budget exhausted ({MAX_EVENTS_SINCE_SNAPSHOT} max since snapshot)"
                            )),
                            custom_effects: vec![],
                            scheduled_actions: vec![],
                            spawn_requests: vec![],
                            spec_governed: true,
                        });
                        return Ok(());
                    }
                    let deleted = EntityEvent {
                        action: "Deleted".to_string(),
                        from_status: state.status.clone(),
                        to_status: "Deleted".to_string(),
                        timestamp: sim_now(),
                        params: serde_json::json!({}),
                        idempotency_key: None,
                    };

                    let (Some(store), Some(backend)) =
                        (self.event_journal.as_ref(), self.event_backend)
                    else {
                        break deleted;
                    };
                    let state_before_delete = state.clone();
                    match self
                        .persist_event(
                            store,
                            backend,
                            &self.persistence_id(),
                            &table,
                            &state_before_delete,
                            state,
                            &deleted,
                            None,
                        )
                        .await
                    {
                        Ok(_) => break deleted,
                        Err(error)
                            if durable_conflict_sequence(&error, state.sequence_nr).is_some()
                                && retries < MAX_DELETE_RETRIES =>
                        {
                            let actual = durable_conflict_sequence(&error, state.sequence_nr)
                                .expect("guard accepted durable conflict");
                            let recovered = self
                                .recover_authoritative_state(store, backend, &table)
                                .await
                                .map_err(|recovery_error| {
                                    ActorError::custom(format!(
                                        "failed to catch up {}:{} after delete concurrency loss: {recovery_error}",
                                        self.entity_type, self.entity_id
                                    ))
                                })?;
                            if recovered.sequence_nr < actual {
                                return Err(ActorError::custom(format!(
                                    "delete catch-up under-reached authoritative sequence for {}:{} ({} < {actual})",
                                    self.entity_type, self.entity_id, recovered.sequence_nr
                                )));
                            }
                            *state = recovered;
                            retries += 1;
                            continue;
                        }
                        Err(error) => {
                            ctx.reply(EntityResponse {
                                success: false,
                                state: state.clone(),
                                error: Some(format!("persistence failed: {error}")),
                                custom_effects: vec![],
                                scheduled_actions: vec![],
                                spawn_requests: vec![],
                                spec_governed: true,
                            });
                            return Ok(());
                        }
                    }
                };

                state.status = deleted.to_status.clone();
                if let Some(obj) = state.fields.as_object_mut() {
                    obj.insert(
                        "Status".to_string(),
                        serde_json::Value::String(state.status.clone()),
                    );
                }
                state.push_event_bounded(deleted);
                self.record_state_key_contract(&table);

                ctx.reply(EntityResponse {
                    success: true,
                    state: state.clone(),
                    error: None,
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    spec_governed: true,
                });
            }
        }
        Ok(())
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

#[cfg(test)]
#[path = "actor_test.rs"]
mod tests;
