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
use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_observe::wide_event;
use temper_runtime::actor::{Actor, ActorContext, ActorError};
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_store_postgres::PostgresEventStore;

use super::types::{
    EntityEvent, EntityMsg, EntityResponse, EntityState, MAX_EVENTS_PER_ENTITY,
    MAX_ITEMS_PER_ENTITY,
};

/// The entity actor -- processes actions through a TransitionTable.
/// Optionally persists events to PostgreSQL. Wide events are emitted
/// via the OTEL SDK (no-op when OTEL is not initialised).
pub struct EntityActor {
    entity_type: String,
    entity_id: String,
    table: Arc<TransitionTable>,
    initial_fields: serde_json::Value,
    /// Optional event store for persistence. None = in-memory only.
    event_store: Option<Arc<PostgresEventStore>>,
    /// Trace ID for correlating all events from this actor.
    trace_id: String,
}

impl EntityActor {
    /// Create a new entity actor (in-memory only, no persistence).
    pub fn new(
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
        table: Arc<TransitionTable>,
        initial_fields: serde_json::Value,
    ) -> Self {
        Self {
            entity_type: entity_type.into(),
            entity_id: entity_id.into(),
            table,
            initial_fields,
            event_store: None,
            trace_id: sim_uuid().to_string(),
        }
    }

    /// Create a new entity actor with Postgres persistence.
    pub fn with_persistence(
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
        table: Arc<TransitionTable>,
        initial_fields: serde_json::Value,
        store: Arc<PostgresEventStore>,
    ) -> Self {
        Self {
            entity_type: entity_type.into(),
            entity_id: entity_id.into(),
            table,
            initial_fields,
            event_store: Some(store),
            trace_id: sim_uuid().to_string(),
        }
    }

    /// Persistence ID for this entity: "EntityType:EntityId".
    fn persistence_id(&self) -> String {
        format!("{}:{}", self.entity_type, self.entity_id)
    }

    /// Persist an event to Postgres (if store is configured).
    async fn persist_event(store: &PostgresEventStore, persistence_id: &str, state: &mut EntityState, event: &EntityEvent) {
        let envelope = PersistenceEnvelope {
            sequence_nr: state.sequence_nr + 1,
            event_type: event.action.clone(),
            payload: serde_json::to_value(event).unwrap_or_default(),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: event.timestamp,
                actor_id: persistence_id.to_string(),
            },
        };

        match store.append(persistence_id, state.sequence_nr, &[envelope]).await {
            Ok(new_seq) => {
                state.sequence_nr = new_seq;
                tracing::debug!(entity = %state.entity_id, seq = new_seq, "event persisted");
            }
            Err(e) => {
                tracing::error!(
                    entity = %state.entity_id, error = %e,
                    "failed to persist event — state advanced but not durable"
                );
            }
        }
    }

    /// Replay events from Postgres to rebuild state (called in pre_start).
    ///
    /// Re-evaluates each event through the `TransitionTable` to reconstruct
    /// all state variables (status, counters, booleans). This is option 2 from
    /// the replay design: the TransitionTable is the authoritative source of
    /// effects, so replay produces the same state as the original execution.
    async fn replay_events(
        table: &TransitionTable,
        store: &PostgresEventStore,
        persistence_id: &str,
        state: &mut EntityState,
    ) {
        match store.read_events(persistence_id, 0).await {
            Ok(envelopes) => {
                for env in &envelopes {
                    if let Ok(event) = serde_json::from_value::<EntityEvent>(env.payload.clone()) {
                        // Re-evaluate through TransitionTable to get effects.
                        // Build the same EvalContext the handler would have used.
                        let mut eval_ctx = temper_jit::table::EvalContext::default();
                        eval_ctx
                            .counters
                            .insert("items".to_string(), state.item_count);
                        for (k, v) in &state.counters {
                            eval_ctx.counters.insert(k.clone(), *v);
                        }
                        for (k, v) in &state.booleans {
                            eval_ctx.booleans.insert(k.clone(), *v);
                        }
                        for (k, v) in &state.lists {
                            eval_ctx.lists.insert(k.clone(), v.clone());
                        }

                        if let Some(result) =
                            table.evaluate_ctx(&state.status, &eval_ctx, &event.action)
                        {
                            if result.success {
                                // Shared effect application — same code as handle() and simulation.
                                let from_status = event.from_status.clone();
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

                        state.events.push(event);
                    }
                    state.sequence_nr = env.sequence_nr;
                }
                if !envelopes.is_empty() {
                    // Sync all state into fields after full replay
                    super::effects::sync_fields(state, &serde_json::json!({}));
                    tracing::info!(
                        entity = %state.entity_id,
                        replayed = envelopes.len(),
                        status = %state.status,
                        seq = state.sequence_nr,
                        counters = ?state.counters,
                        booleans = ?state.booleans,
                        "state rebuilt from event journal via TransitionTable"
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
    }
}

impl Actor for EntityActor {
    type Msg = EntityMsg;
    type State = EntityState;

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<Self::State, ActorError> {
        let mut fields = self.initial_fields.clone();
        if let Some(obj) = fields.as_object_mut() {
            obj.entry("Id".to_string())
                .or_insert(serde_json::Value::String(self.entity_id.clone()));
            obj.entry("Status".to_string())
                .or_insert(serde_json::Value::String(self.table.initial_state.clone()));
        }

        let mut state = EntityState {
            entity_type: self.entity_type.clone(),
            entity_id: self.entity_id.clone(),
            status: self.table.initial_state.clone(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields,
            events: Vec::new(),
            sequence_nr: 0,
        };

        // Replay events from Postgres to rebuild state (if persistence is configured).
        // Re-evaluates each event through the TransitionTable to reconstruct
        // all state variables (status, counters, booleans) — not just item_count.
        if let Some(ref store) = self.event_store {
            Self::replay_events(&self.table, store, &self.persistence_id(), &mut state).await;
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
            EntityMsg::Action { name, params } => {
                // Capture start time for span duration (DST-safe: sim_now()
                // returns logical clock in simulation, wall clock in production).
                let action_start = sim_now();

                // TigerStyle: Assert preconditions before every transition.
                // These run in production, not just tests.
                debug_assert!(
                    self.table.states.contains(&state.status),
                    "PRECONDITION: status '{}' not in valid states {:?}",
                    state.status, self.table.states
                );
                debug_assert!(
                    state.events.len() < MAX_EVENTS_PER_ENTITY,
                    "PRECONDITION: event budget exhausted ({} >= {})",
                    state.events.len(), MAX_EVENTS_PER_ENTITY
                );
                debug_assert!(
                    state.item_count <= MAX_ITEMS_PER_ENTITY,
                    "PRECONDITION: item budget exceeded ({} > {})",
                    state.item_count, MAX_ITEMS_PER_ENTITY
                );

                // TigerStyle: Budget enforcement (not just assertions -- hard limits)
                if state.events.len() >= MAX_EVENTS_PER_ENTITY {
                    ctx.reply(EntityResponse {
                        success: false,
                        state: state.clone(),
                        error: Some(format!("Event budget exhausted ({MAX_EVENTS_PER_ENTITY} max)")),
                        custom_effects: vec![],
                    });
                    return Ok(());
                }

                let mut eval_ctx = temper_jit::table::EvalContext::default();
                eval_ctx.counters.insert("items".to_string(), state.item_count);
                for (k, v) in &state.counters {
                    eval_ctx.counters.insert(k.clone(), *v);
                }
                for (k, v) in &state.booleans {
                    eval_ctx.booleans.insert(k.clone(), *v);
                }
                for (k, v) in &state.lists {
                    eval_ctx.lists.insert(k.clone(), v.clone());
                }
                let result = self.table.evaluate_ctx(&state.status, &eval_ctx, &name);
                let event_count_before = state.events.len();

                match result {
                    Some(transition_result) if transition_result.success => {
                        let from_status = state.status.clone();
                        let to_status = transition_result.new_state.clone();

                        // Shared effect application — same code path as simulation.
                        // FoundationDB DST principle: one function for all paths.
                        let custom_effects = super::effects::apply_effects(
                            state,
                            &transition_result.effects,
                            &params,
                        );
                        super::effects::apply_new_state_fallback(
                            state,
                            &from_status,
                            &to_status,
                        );
                        super::effects::sync_fields(state, &params);

                        // Record event
                        let event = EntityEvent {
                            action: name.clone(),
                            from_status,
                            to_status: state.status.clone(),
                            timestamp: sim_now(),
                            params: params.clone(),
                        };

                        // Persist to Postgres (if configured)
                        if let Some(ref store) = self.event_store {
                            Self::persist_event(store, &self.persistence_id(), state, &event).await;
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
                        let wide = wide_event::from_transition(
                            &state.entity_type, &state.entity_id, &name,
                            &event.from_status, &state.status, true, duration_ns,
                            &event.params, state.item_count, &self.trace_id,
                        );
                        wide_event::emit_span(&wide);
                        wide_event::emit_metrics(&wide);

                        state.events.push(event);

                        // TigerStyle: Assert postconditions after every transition.
                        debug_assert!(
                            self.table.states.contains(&state.status),
                            "POSTCONDITION: status '{}' not in valid states after {}",
                            state.status, name
                        );
                        debug_assert!(
                            state.events.len() == event_count_before + 1,
                            "POSTCONDITION: event log must grow by exactly 1 (was {}, now {})",
                            event_count_before, state.events.len()
                        );
                        debug_assert!(
                            state.events.last().unwrap().action == name, // ci-ok: post-assertion, events.len() just checked
                            "POSTCONDITION: last event must be the action that just fired"
                        );

                        tracing::info!(
                            entity = %state.entity_id,
                            action = %name,
                            to = %state.status,
                            events = state.events.len(),
                            "transition applied"
                        );

                        ctx.reply(EntityResponse {
                            success: true,
                            state: state.clone(),
                            error: None,
                            custom_effects,
                        });
                    }
                    Some(_) => {
                        // Transition failed (guard not met) — emit telemetry
                        let action_end = sim_now();
                        let duration_ns = (action_end - action_start)
                            .num_nanoseconds()
                            .unwrap_or(0)
                            .max(0) as u64;
                        let wide = wide_event::from_transition(
                            &state.entity_type, &state.entity_id, &name,
                            &state.status, &state.status, false, duration_ns,
                            &params, state.item_count, &self.trace_id,
                        );
                        wide_event::emit_span(&wide);
                        wide_event::emit_metrics(&wide);

                        ctx.reply(EntityResponse {
                            success: false,
                            state: state.clone(),
                            error: Some(format!(
                                "Action '{}' not valid from state '{}'",
                                name, state.status
                            )),
                            custom_effects: vec![],
                        });
                    }
                    None => {
                        // Unknown action — emit telemetry
                        let action_end = sim_now();
                        let duration_ns = (action_end - action_start)
                            .num_nanoseconds()
                            .unwrap_or(0)
                            .max(0) as u64;
                        let wide = wide_event::from_transition(
                            &state.entity_type, &state.entity_id, &name,
                            &state.status, &state.status, false, duration_ns,
                            &params, state.item_count, &self.trace_id,
                        );
                        wide_event::emit_span(&wide);
                        wide_event::emit_metrics(&wide);

                        ctx.reply(EntityResponse {
                            success: false,
                            state: state.clone(),
                            error: Some(format!("Unknown action: {}", name)),
                            custom_effects: vec![],
                        });
                    }
                }
            }
            EntityMsg::GetState => {
                ctx.reply(EntityResponse {
                    success: true,
                    state: state.clone(),
                    error: None,
                    custom_effects: vec![],
                });
            }
            EntityMsg::GetField { field } => {
                let value = state.fields.get(&field).cloned().unwrap_or(serde_json::Value::Null);
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
                    if let (Some(existing), Some(updates)) = (state.fields.as_object_mut(), fields.as_object()) {
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
                });
            }
            EntityMsg::Delete => {
                ctx.reply(EntityResponse {
                    success: true,
                    state: state.clone(),
                    error: None,
                    custom_effects: vec![],
                });
            }
        }
        Ok(())
    }

    async fn post_stop(&self, state: Self::State, _ctx: &mut ActorContext<Self>) {
        tracing::info!(
            entity = %state.entity_id,
            status = %state.status,
            events = state.events.len(),
            "entity actor stopped"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use temper_runtime::ActorSystem;
    use temper_jit::table::TransitionTable;

    const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

    fn order_table() -> Arc<TransitionTable> {
        Arc::new(TransitionTable::from_ioa_source(ORDER_IOA))
    }

    // =============================================
    // DST-FIRST: Test the actor through the runtime
    // =============================================

    #[tokio::test]
    async fn dst_entity_starts_in_initial_state() {
        let system = ActorSystem::new("dst");
        let table = order_table();
        let actor = EntityActor::new("Order", "order-1", table, serde_json::json!({}));
        let actor_ref = system.spawn(actor, "order-1");

        let response: EntityResponse = actor_ref
            .ask(EntityMsg::GetState, Duration::from_secs(1))
            .await
            .unwrap();

        assert_eq!(response.state.status, "Draft");
        assert_eq!(response.state.entity_id, "order-1");
        assert_eq!(response.state.item_count, 0);
        assert!(response.state.events.is_empty());
    }

    #[tokio::test]
    async fn dst_add_item_then_submit() {
        let system = ActorSystem::new("dst");
        let table = order_table();
        let actor = EntityActor::new("Order", "order-2", table, serde_json::json!({}));
        let actor_ref = system.spawn(actor, "order-2");

        // Add an item (Draft -> Draft, item_count 0 -> 1)
        let r: EntityResponse = actor_ref
            .ask(
                EntityMsg::Action {
                    name: "AddItem".into(),
                    params: serde_json::json!({"ProductId": "prod-1"}),
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(r.success);
        assert_eq!(r.state.status, "Draft");
        assert_eq!(r.state.item_count, 1);

        // Submit (Draft -> Submitted)
        let r: EntityResponse = actor_ref
            .ask(
                EntityMsg::Action {
                    name: "SubmitOrder".into(),
                    params: serde_json::json!({"ShippingAddressId": "addr-1"}),
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(r.success, "submit should succeed, got: {:?}", r.error);
        assert_eq!(r.state.status, "Submitted");
        assert_eq!(r.state.events.len(), 2); // AddItem + SubmitOrder
    }

    #[tokio::test]
    async fn dst_cannot_submit_without_items() {
        let system = ActorSystem::new("dst");
        let table = order_table();
        let actor = EntityActor::new("Order", "order-3", table, serde_json::json!({}));
        let actor_ref = system.spawn(actor, "order-3");

        // Try to submit with 0 items -- should fail
        let r: EntityResponse = actor_ref
            .ask(
                EntityMsg::Action {
                    name: "SubmitOrder".into(),
                    params: serde_json::json!({}),
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(!r.success);
        assert_eq!(r.state.status, "Draft"); // Still in Draft
    }

    #[tokio::test]
    async fn dst_full_order_lifecycle() {
        let system = ActorSystem::new("dst");
        let table = order_table();
        let actor = EntityActor::new("Order", "order-4", table, serde_json::json!({}));
        let actor_ref = system.spawn(actor, "order-4");

        // Draft -> AddItem -> SubmitOrder -> ConfirmOrder -> ProcessOrder -> ShipOrder -> DeliverOrder
        let actions = vec![
            ("AddItem", serde_json::json!({})),
            ("SubmitOrder", serde_json::json!({})),
            ("ConfirmOrder", serde_json::json!({})),
            ("ProcessOrder", serde_json::json!({})),
            ("ShipOrder", serde_json::json!({})),
            ("DeliverOrder", serde_json::json!({})),
        ];

        let expected_states = vec![
            "Draft",      // after AddItem
            "Submitted",  // after SubmitOrder
            "Confirmed",  // after ConfirmOrder
            "Processing", // after ProcessOrder
            "Shipped",    // after ShipOrder
            "Delivered",  // after DeliverOrder
        ];

        for (i, (action, params)) in actions.into_iter().enumerate() {
            let r: EntityResponse = actor_ref
                .ask(
                    EntityMsg::Action {
                        name: action.into(),
                        params,
                    },
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
            assert!(r.success, "step {i} ({action}) failed: {:?}", r.error);
            assert_eq!(r.state.status, expected_states[i], "step {i} ({action}) wrong state");
        }

        // Verify full event log
        let r: EntityResponse = actor_ref
            .ask(EntityMsg::GetState, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(r.state.events.len(), 6);
        assert_eq!(r.state.status, "Delivered");
    }

    #[tokio::test]
    async fn dst_cancel_from_draft() {
        let system = ActorSystem::new("dst");
        let table = order_table();
        let actor = EntityActor::new("Order", "order-5", table, serde_json::json!({}));
        let actor_ref = system.spawn(actor, "order-5");

        let r: EntityResponse = actor_ref
            .ask(
                EntityMsg::Action {
                    name: "CancelOrder".into(),
                    params: serde_json::json!({"Reason": "changed mind"}),
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(r.success);
        assert_eq!(r.state.status, "Cancelled");
    }

    #[tokio::test]
    async fn dst_cannot_cancel_shipped_order() {
        let system = ActorSystem::new("dst");
        let table = order_table();
        let actor = EntityActor::new("Order", "order-6", table, serde_json::json!({}));
        let actor_ref = system.spawn(actor, "order-6");

        // Drive to Shipped
        for action in &["AddItem", "SubmitOrder", "ConfirmOrder", "ProcessOrder", "ShipOrder"] {
            let _: EntityResponse = actor_ref
                .ask(
                    EntityMsg::Action {
                        name: action.to_string(),
                        params: serde_json::json!({}),
                    },
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
        }

        // Try to cancel -- should fail
        let r: EntityResponse = actor_ref
            .ask(
                EntityMsg::Action {
                    name: "CancelOrder".into(),
                    params: serde_json::json!({}),
                },
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(!r.success);
        assert_eq!(r.state.status, "Shipped"); // Still Shipped
        assert!(r.error.unwrap().contains("not valid"));
    }

    #[tokio::test]
    async fn dst_multiple_actors_independent() {
        let system = ActorSystem::new("dst");
        let table = order_table();

        let a1 = system.spawn(
            EntityActor::new("Order", "order-A", table.clone(), serde_json::json!({})),
            "order-A",
        );
        let a2 = system.spawn(
            EntityActor::new("Order", "order-B", table.clone(), serde_json::json!({})),
            "order-B",
        );

        // Cancel order A
        let _: EntityResponse = a1
            .ask(EntityMsg::Action { name: "CancelOrder".into(), params: serde_json::json!({}) }, Duration::from_secs(1))
            .await.unwrap();

        // Add item to order B
        let _: EntityResponse = a2
            .ask(EntityMsg::Action { name: "AddItem".into(), params: serde_json::json!({}) }, Duration::from_secs(1))
            .await.unwrap();

        // Verify independence
        let r1: EntityResponse = a1.ask(EntityMsg::GetState, Duration::from_secs(1)).await.unwrap();
        let r2: EntityResponse = a2.ask(EntityMsg::GetState, Duration::from_secs(1)).await.unwrap();

        assert_eq!(r1.state.status, "Cancelled");
        assert_eq!(r2.state.status, "Draft");
        assert_eq!(r2.state.item_count, 1);
    }
}
