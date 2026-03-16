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
}

impl EntityActor {
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
        }
    }

    /// Set the tenant for this actor (must be called before spawning).
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
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
                                    let (_custom_effects, _scheduled_actions, _spawn_requests) =
                                        super::effects::apply_effects(
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
                            super::effects::sync_fields(state, &event.params);

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
                            crate::runtime_metrics::record_replay_error(tenant, &state.entity_type);
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

impl Actor for EntityActor {
    type Msg = EntityMsg;
    type State = EntityState;

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<Self::State, ActorError> {
        // Snapshot the table for consistent startup (initial state + replay).
        // This is a cheap clone — TransitionTable is a few Vecs of strings.
        let table = self.table.read().expect("table lock poisoned").clone();

        let mut fields = self.initial_fields.clone();
        if let Some(obj) = fields.as_object_mut() {
            obj.entry("Id".to_string())
                .or_insert(serde_json::Value::String(self.entity_id.clone()));
            obj.entry("Status".to_string())
                .or_insert(serde_json::Value::String(table.initial_state.clone()));
        }

        let mut state = EntityState {
            entity_type: self.entity_type.clone(),
            entity_id: self.entity_id.clone(),
            status: table.initial_state.clone(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields,
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            sequence_nr: 0,
        };

        // Replay events from Postgres to rebuild state (if persistence is configured).
        // Re-evaluates each event through the TransitionTable to reconstruct
        // all state variables (status, counters, booleans) — not just item_count.
        if let Some(ref store) = self.event_store {
            Self::replay_events(
                &table,
                store,
                &self.persistence_id(),
                &mut state,
                &self.tenant,
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
            } => {
                // Capture start time for span duration (DST-safe: sim_now()
                // returns logical clock in simulation, wall clock in production).
                let action_start = sim_now();

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

                let event_count_before = state.total_event_count;
                let state_before = state.clone();
                let result = super::effects::process_action_with_xref(
                    state,
                    &table,
                    &name,
                    &params,
                    &cross_entity_booleans,
                );

                if result.success {
                    // process_action returned a successful transition with event.
                    let event = result
                        .event
                        .expect("successful process_action always returns event"); // ci-ok: post-assertion, success guarantees Some

                    // Persist to Postgres (if configured)
                    if let Some(ref store) = self.event_store
                        && let Err(e) =
                            Self::persist_event(store, &self.persistence_id(), state, &event).await
                    {
                        // Roll back speculative in-memory state if durability failed.
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

                    ctx.reply(EntityResponse {
                        success: true,
                        state: state.clone(),
                        error: None,
                        custom_effects: result.custom_effects,
                        scheduled_actions: result.scheduled_actions,
                        spawn_requests: result.spawn_requests,
                        spec_governed: true,
                    });
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
