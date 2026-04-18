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

use temper_jit::table::TransitionTable;
use temper_observe::wide_event;
use temper_runtime::actor::{Actor, ActorContext, ActorError};
use temper_runtime::persistence::{
    EventMetadata, EventStore, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};

use crate::event_store::ServerEventStore;

use super::effects::{FieldSyncMode, process_action_with_xref_and_field_mode};
use super::types::{
    EntityEvent, EntityMsg, EntityResponse, EntityState, MAX_EVENTS_PER_ENTITY,
    MAX_ITEMS_PER_ENTITY,
};

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
    /// Optional event store for persistence. None = in-memory only.
    event_store: Option<Arc<ServerEventStore>>,
    /// Trace ID for correlating all events from this actor.
    trace_id: String,
    /// Shared idempotency cache (ADR-0048 sub-decision 5). Consulted before
    /// executing an action whose `idempotency_key` is set, so dispatch-layer
    /// retries that race past the caller's timeout cannot double-execute.
    idempotency_cache: Option<Arc<crate::idempotency::IdempotencyCache>>,
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
            total_event_count: 0,
            sequence_nr: 0,
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
    fn serialize_snapshot_state(state: &EntityState) -> Result<Vec<u8>, PersistenceError> {
        let mut value = serde_json::to_value(state)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        if let Some(obj) = value.as_object_mut() {
            obj.remove("events");
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

        match serde_json::from_value::<EntityState>(value) {
            Ok(mut restored) => {
                restored.sequence_nr = sequence_nr;
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
            event_store: None,
            trace_id: sim_uuid().to_string(),
            idempotency_cache: None,
        }
    }

    /// Create a new entity actor with persistence.
    pub fn with_persistence(
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
        table: Arc<RwLock<TransitionTable>>,
        initial_fields: serde_json::Value,
        store: Arc<ServerEventStore>,
    ) -> Self {
        Self {
            tenant: "default".into(),
            entity_type: entity_type.into(),
            entity_id: entity_id.into(),
            table,
            initial_fields,
            event_store: Some(store),
            trace_id: sim_uuid().to_string(),
            idempotency_cache: None,
        }
    }

    /// Set the tenant for this actor (must be called before spawning).
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
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

    /// Persistence ID for this entity: "tenant:EntityType:EntityId".
    fn persistence_id(&self) -> String {
        format!("{}:{}:{}", self.tenant, self.entity_type, self.entity_id)
    }

    /// Persist an event to the configured event store.
    async fn persist_event(
        store: &ServerEventStore,
        persistence_id: &str,
        state: &mut EntityState,
        event: &EntityEvent,
    ) -> Result<u64, PersistenceError> {
        let payload = serde_json::to_value(event)
            .map_err(|e| PersistenceError::Serialization(e.to_string()))?;
        let envelope = PersistenceEnvelope {
            sequence_nr: state.sequence_nr + 1,
            event_type: event.action.clone(),
            payload,
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: event.timestamp,
                actor_id: persistence_id.to_string(),
            },
        };

        match store
            .append(persistence_id, state.sequence_nr, &[envelope])
            .await
        {
            Ok(new_seq) => {
                state.sequence_nr = new_seq;
                tracing::debug!(entity = %state.entity_id, seq = new_seq, "event persisted");
                Ok(new_seq)
            }
            Err(e) => {
                tracing::error!(
                    entity = %state.entity_id, error = %e,
                    "failed to persist event — state advanced but not durable"
                );
                Err(e)
            }
        }
    }

    /// Save a snapshot when the configured interval is reached.
    async fn maybe_save_snapshot(
        store: &ServerEventStore,
        persistence_id: &str,
        state: &EntityState,
    ) -> Result<(), PersistenceError> {
        if state.sequence_nr == 0 {
            return Ok(());
        }
        let interval = Self::snapshot_interval();
        if !state.sequence_nr.is_multiple_of(interval) {
            return Ok(());
        }

        let snapshot = Self::serialize_snapshot_state(state)?;
        store
            .save_snapshot(persistence_id, state.sequence_nr, &snapshot)
            .await
    }

    /// Replay events from the configured store to rebuild state (called in pre_start).
    ///
    /// Re-evaluates each event through the `TransitionTable` to reconstruct
    /// all state variables (status, counters, booleans). This is option 2 from
    /// the replay design: the TransitionTable is the authoritative source of
    /// effects, so replay produces the same state as the original execution.
    async fn replay_events(
        table: &TransitionTable,
        store: &ServerEventStore,
        persistence_id: &str,
        state: &mut EntityState,
        tenant: &str,
    ) {
        let replay_start = Instant::now(); // determinism-ok: wall-clock for production replay duration metric only
        let mut from_sequence = 0;
        let mut loaded_snapshot = false;

        match store.load_snapshot(persistence_id).await {
            Ok(Some((snapshot_seq, snapshot_bytes))) => {
                if Self::apply_snapshot_bytes(state, snapshot_seq, &snapshot_bytes) {
                    from_sequence = snapshot_seq;
                    loaded_snapshot = true;
                    tracing::info!(
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

        match store.read_events(persistence_id, from_sequence).await {
            Ok(envelopes) => {
                for env in &envelopes {
                    let parsed_event = serde_json::from_value::<EntityEvent>(env.payload.clone());

                    // Tombstone is terminal: once deleted, entity must not replay
                    // into a live state. Stop at the first Deleted event.
                    if env.event_type == "Deleted" {
                        let tombstone = parsed_event.unwrap_or_else(|_| EntityEvent {
                            action: "Deleted".to_string(),
                            from_status: state.status.clone(),
                            to_status: "Deleted".to_string(),
                            timestamp: env.metadata.timestamp,
                            params: serde_json::json!({}),
                        });
                        state.status = tombstone.to_status.clone();
                        if let Some(obj) = state.fields.as_object_mut() {
                            obj.insert(
                                "Status".to_string(),
                                serde_json::Value::String(state.status.clone()),
                            );
                        }
                        state.push_event_bounded(tombstone);
                        state.sequence_nr = env.sequence_nr;
                        break;
                    }

                    match parsed_event {
                        Ok(event) => {
                            // Re-evaluate through TransitionTable to get effects.
                            // Build the same EvalContext the handler would have used.
                            let eval_ctx = super::effects::build_eval_context(state);

                            if let Some(result) =
                                table.evaluate_ctx(&state.status, &eval_ctx, &event.action)
                            {
                                if result.success {
                                    // Shared effect application — same code as handle() and simulation.
                                    let from_status = event.from_status.clone();
                                    let (
                                        _custom_effects,
                                        _scheduled_actions,
                                        _spawn_requests,
                                        _schedule_at_requests,
                                    ) = super::effects::apply_effects(
                                        state,
                                        &result.effects,
                                        &event.params,
                                    );
                                    super::effects::apply_new_state_fallback(
                                        state,
                                        &from_status,
                                        &result.new_state,
                                    );
                                }
                            } else {
                                // TransitionTable doesn't know this action — fall back
                                // to the stored to_status (safe: status is always stored).
                                state.status = event.to_status.clone();
                            }

                            // Sync action params into fields — mirrors the live
                            // process_action() path (effects.rs:155) so data fields
                            // like Title, Description, Priority survive replay.
                            let field_sync_mode = match store {
                                ServerEventStore::Turso(_) | ServerEventStore::TenantRouted(_) => {
                                    FieldSyncMode::blob_refs_default()
                                }
                                _ => FieldSyncMode::InlineTruncate,
                            };
                            let overflow_blobs = super::effects::sync_fields_with_metadata(
                                state,
                                &event.params,
                                field_sync_mode,
                                Some(&table.state_var_metadata),
                            );
                            // Persist replayed overflow blobs so blob-ref envelopes
                            // resolve on subsequent OData reads. Content-addressed
                            // dedup makes this idempotent — if the original live
                            // action already persisted the blob, INSERT OR IGNORE
                            // is a no-op. If the prior server died between emitting
                            // the event and persisting the blob, this is the
                            // recovery path. See ADR-0040, ADR-0045.
                            if !overflow_blobs.is_empty() {
                                if let Some(blob_store) = store.turso_for_tenant(tenant).await {
                                    if let Err(e) = crate::blobs::put_overflow_blobs(
                                        &blob_store,
                                        &overflow_blobs,
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            entity = %state.entity_id,
                                            error = %e,
                                            overflow_count = overflow_blobs.len(),
                                            "failed to persist replayed overflow blobs — blob-ref envelopes may dangle"
                                        );
                                    }
                                } else {
                                    tracing::warn!(
                                        entity = %state.entity_id,
                                        overflow_count = overflow_blobs.len(),
                                        "replay produced overflow blobs but no Turso store available for tenant"
                                    );
                                }
                            }

                            state.push_event_bounded(event);
                        }
                        Err(e) => {
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
                if !envelopes.is_empty() {
                    tracing::info!(
                        entity = %state.entity_id,
                        snapshot_loaded = loaded_snapshot,
                        replayed = envelopes.len(),
                        status = %state.status,
                        seq = state.sequence_nr,
                        total_events = state.total_event_count,
                        recent_events = state.events.len(),
                        counters = ?state.counters,
                        booleans = ?state.booleans,
                        "state rebuilt from event journal via TransitionTable"
                    );
                } else if loaded_snapshot {
                    tracing::info!(
                        entity = %state.entity_id,
                        seq = state.sequence_nr,
                        total_events = state.total_event_count,
                        "state restored from snapshot (no delta events)"
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    entity = %state.entity_id, error = %e,
                    "failed to read events for replay — starting fresh"
                );
            }
        }
        crate::runtime_metrics::record_event_replay_duration(
            replay_start.elapsed(),
            tenant,
            &state.entity_type,
        );
    }
}

pub(crate) async fn recover_entity_state_from_store(
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    store: &ServerEventStore,
    initial_fields: &serde_json::Value,
) -> EntityState {
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
    let mut state = EntityActor::build_initial_state(entity_type, entity_id, table, initial_fields);
    EntityActor::replay_events(table, store, &persistence_id, &mut state, tenant).await;
    state
}

impl Actor for EntityActor {
    type Msg = EntityMsg;
    type State = EntityState;

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<Self::State, ActorError> {
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
        if let Some(ref store) = self.event_store {
            state = recover_entity_state_from_store(
                &self.tenant,
                &self.entity_type,
                &self.entity_id,
                &table,
                store,
                &self.initial_fields,
            )
            .await;
        }

        // Persist a bootstrap Created event for first-time entities so initial
        // fields are durable and replayable.
        if self.event_store.is_some() && state.total_event_count == 0 {
            let created = EntityEvent {
                action: "Created".to_string(),
                from_status: String::new(),
                to_status: state.status.clone(),
                timestamp: sim_now(),
                params: self.initial_fields.clone(),
            };

            if let Some(ref store) = self.event_store {
                Self::persist_event(store, &self.persistence_id(), &mut state, &created)
                    .await
                    .map_err(|e| {
                        ActorError::custom(format!(
                            "failed to persist bootstrap Created event for {}:{}: {}",
                            self.entity_type, self.entity_id, e
                        ))
                    })?;
            }
            state.push_event_bounded(created);
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

                // Snapshot the current table for this action dispatch.
                // On the next action, any hot-swapped table will be picked up.
                let table = self.table.read().expect("table lock poisoned").clone();

                // TigerStyle: Assert preconditions before every transition.
                // These run in production, not just tests.
                debug_assert!(
                    table.states.contains(&state.status),
                    "PRECONDITION: status '{}' not in valid states {:?}",
                    state.status,
                    table.states
                );
                debug_assert!(
                    state.total_event_count < MAX_EVENTS_PER_ENTITY,
                    "PRECONDITION: event budget exhausted ({} >= {})",
                    state.total_event_count,
                    MAX_EVENTS_PER_ENTITY
                );
                debug_assert!(
                    state.item_count <= MAX_ITEMS_PER_ENTITY,
                    "PRECONDITION: item budget exceeded ({} > {})",
                    state.item_count,
                    MAX_ITEMS_PER_ENTITY
                );

                // TigerStyle: Budget enforcement (not just assertions -- hard limits)
                if state.total_event_count >= MAX_EVENTS_PER_ENTITY {
                    ctx.reply(EntityResponse {
                        success: false,
                        state: state.clone(),
                        error: Some(format!(
                            "Event budget exhausted ({MAX_EVENTS_PER_ENTITY} max)"
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
                let field_sync_mode = match self.event_store.as_ref().map(Arc::as_ref) {
                    Some(ServerEventStore::Turso(_) | ServerEventStore::TenantRouted(_)) => {
                        FieldSyncMode::blob_refs_default()
                    }
                    _ => FieldSyncMode::InlineTruncate,
                };

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

                    if !result.overflow_blobs.is_empty() {
                        let Some(ref store) = self.event_store else {
                            *state = state_before;
                            ctx.reply(EntityResponse {
                                success: false,
                                state: state.clone(),
                                error: Some(
                                    "field-overflow blobs require a Turso-backed store".to_string(),
                                ),
                                custom_effects: vec![],
                                scheduled_actions: vec![],
                                spawn_requests: vec![],
                                spec_governed: true,
                            });
                            return Ok(());
                        };

                        let Some(blob_store) = store.turso_for_tenant(&self.tenant).await else {
                            *state = state_before;
                            ctx.reply(EntityResponse {
                                success: false,
                                state: state.clone(),
                                error: Some(
                                    "field-overflow blobs require a Turso-backed store".to_string(),
                                ),
                                custom_effects: vec![],
                                scheduled_actions: vec![],
                                spawn_requests: vec![],
                                spec_governed: true,
                            });
                            return Ok(());
                        };

                        if let Err(e) =
                            crate::blobs::put_overflow_blobs(&blob_store, &result.overflow_blobs)
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
                    }

                    // Persist to Postgres (if configured). On
                    // `ConcurrencyViolation` enter the ADR-0046 retry cycle —
                    // replay events, re-evaluate the action against the caught-up
                    // state, and retry the persist up to two more times. Other
                    // error variants fail immediately (same as before).
                    if let Some(ref store) = self.event_store {
                        let first_persist =
                            Self::persist_event(store, &self.persistence_id(), state, &event).await;

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

                                    // Rollback speculative state.
                                    *state = state_before.clone();

                                    // Catch up to the authoritative sequence.
                                    Self::replay_events(
                                        &table,
                                        store,
                                        &self.persistence_id(),
                                        state,
                                        &self.tenant,
                                    )
                                    .await;

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

                                    // Overflow blobs for the re-evaluated result.
                                    if !retry_result.overflow_blobs.is_empty() {
                                        match store.turso_for_tenant(&self.tenant).await {
                                            Some(blob_store) => {
                                                if let Err(e) = crate::blobs::put_overflow_blobs(
                                                    &blob_store,
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
                                            }
                                            None => {
                                                retry_final = Some((
                                                    crate::runtime_metrics::ConcurrencyRetryOutcome::Exhausted,
                                                    Some(
                                                        "field-overflow blobs require a Turso-backed store"
                                                            .to_string(),
                                                    ),
                                                ));
                                                break;
                                            }
                                        }
                                    }

                                    // Backoff: retry 1 → 10ms, retry 2 → 50ms.
                                    let backoff_ms = if retry_idx == 1 { 10 } else { 50 };
                                    tokio::time::sleep(std::time::Duration::from_millis(
                                        backoff_ms,
                                    ))
                                    .await; // determinism-ok: rare retry backoff (ADR-0046)

                                    match Self::persist_event(
                                        store,
                                        &self.persistence_id(),
                                        state,
                                        &retry_event,
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

                    if let Some(ref store) = self.event_store
                        && let Err(e) =
                            Self::maybe_save_snapshot(store, &self.persistence_id(), state).await
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
            EntityMsg::GetField { field } => {
                let value = state
                    .fields
                    .get(&field)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                ctx.reply(value);
            }
            EntityMsg::UpdateFields { fields, replace } => {
                if replace {
                    // PUT: replace all fields (preserve Id and Status)
                    let id = state.entity_id.clone();
                    let status = state.status.clone();
                    state.fields = fields;
                    if let Some(obj) = state.fields.as_object_mut() {
                        obj.insert("Id".to_string(), serde_json::Value::String(id));
                        obj.insert("Status".to_string(), serde_json::Value::String(status));
                    }
                } else {
                    // PATCH: merge fields into existing
                    if let (Some(existing), Some(updates)) =
                        (state.fields.as_object_mut(), fields.as_object())
                    {
                        for (k, v) in updates {
                            existing.insert(k.clone(), v.clone());
                        }
                    }
                }
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
            EntityMsg::Delete => {
                let deleted = EntityEvent {
                    action: "Deleted".to_string(),
                    from_status: state.status.clone(),
                    to_status: "Deleted".to_string(),
                    timestamp: sim_now(),
                    params: serde_json::json!({}),
                };

                if let Some(ref store) = self.event_store
                    && let Err(e) =
                        Self::persist_event(store, &self.persistence_id(), state, &deleted).await
                {
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

                state.status = deleted.to_status.clone();
                if let Some(obj) = state.fields.as_object_mut() {
                    obj.insert(
                        "Status".to_string(),
                        serde_json::Value::String(state.status.clone()),
                    );
                }
                state.push_event_bounded(deleted);

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
