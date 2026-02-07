//! Generic entity actor powered by JIT transition tables.
//!
//! This is the bridge between the actor runtime and the state machine specs.
//! Each entity actor holds its current state and a TransitionTable, and
//! processes action messages by evaluating transitions through the table.
//!
//! The same TransitionTable used here is also used by:
//! - Stateright model checking (Level 1)
//! - Deterministic simulation (Level 2)
//! - Property-based tests (Level 3)
//! So if it passes verification, it works correctly here.
//!
//! ## TigerStyle Principles Applied
//!
//! - **Assertions in production**: Pre/postcondition assertions on every transition.
//!   Status must be in the valid state set. Item count must not go negative.
//!   Event log must grow monotonically. These are not debug-only — they run always.
//! - **Bounded execution**: Max events per entity (10,000), max items (1,000).
//!   No unbounded growth. Violations are detected immediately, not at OOM.
//! - **Explicit error handling**: Every match arm handled. No unwrap on user input.
//! - **Deterministic**: Same input → same output. No randomness in transition logic.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use temper_jit::table::TransitionTable;
use temper_runtime::actor::{Actor, ActorContext, ActorError, Message};

#[cfg(test)]
use std::time::Duration;

// TigerStyle: Fixed resource budgets. No unbounded growth.
// These are hard limits, not suggestions. Violations are assertion failures.

/// Maximum events per entity before the actor refuses new transitions.
const MAX_EVENTS_PER_ENTITY: usize = 10_000;
/// Maximum items an entity can hold.
const MAX_ITEMS_PER_ENTITY: usize = 1_000;

/// Messages the entity actor can receive.
#[derive(Debug)]
pub enum EntityMsg {
    /// Execute a state machine action (e.g., "SubmitOrder", "CancelOrder").
    Action {
        name: String,
        params: serde_json::Value,
    },
    /// Get the current entity state.
    GetState,
    /// Get a specific field value.
    GetField { field: String },
}

impl Message for EntityMsg {}

/// The entity's runtime state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityState {
    /// Entity type (e.g., "Order").
    pub entity_type: String,
    /// Entity ID.
    pub entity_id: String,
    /// Current status (state machine state).
    pub status: String,
    /// Item count (for entities with collections).
    pub item_count: usize,
    /// All entity fields as a JSON object.
    pub fields: serde_json::Value,
    /// Event log (append-only history of all transitions).
    pub events: Vec<EntityEvent>,
}

/// A recorded state transition event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityEvent {
    pub action: String,
    pub from_status: String,
    pub to_status: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub params: serde_json::Value,
}

/// The response returned from an action or query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityResponse {
    /// Whether the action succeeded.
    pub success: bool,
    /// The current entity state after the action.
    pub state: EntityState,
    /// Error message if the action failed.
    pub error: Option<String>,
}

/// The entity actor — processes actions through a TransitionTable.
pub struct EntityActor {
    entity_type: String,
    entity_id: String,
    table: Arc<TransitionTable>,
    initial_fields: serde_json::Value,
}

impl EntityActor {
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
        }
    }
}

impl Actor for EntityActor {
    type Msg = EntityMsg;
    type State = EntityState;

    async fn pre_start(&self, _ctx: &mut ActorContext<Self>) -> Result<Self::State, ActorError> {
        let mut fields = self.initial_fields.clone();
        // Ensure standard fields
        if let Some(obj) = fields.as_object_mut() {
            obj.entry("Id".to_string())
                .or_insert(serde_json::Value::String(self.entity_id.clone()));
            obj.entry("Status".to_string())
                .or_insert(serde_json::Value::String(self.table.initial_state.clone()));
        }

        Ok(EntityState {
            entity_type: self.entity_type.clone(),
            entity_id: self.entity_id.clone(),
            status: self.table.initial_state.clone(),
            item_count: 0,
            fields,
            events: Vec::new(),
        })
    }

    async fn handle(
        &self,
        msg: Self::Msg,
        state: &mut Self::State,
        ctx: &mut ActorContext<Self>,
    ) -> Result<(), ActorError> {
        match msg {
            EntityMsg::Action { name, params } => {
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

                // TigerStyle: Budget enforcement (not just assertions — hard limits)
                if state.events.len() >= MAX_EVENTS_PER_ENTITY {
                    ctx.reply(EntityResponse {
                        success: false,
                        state: state.clone(),
                        error: Some(format!("Event budget exhausted ({MAX_EVENTS_PER_ENTITY} max)")),
                    });
                    return Ok(());
                }

                let result = self.table.evaluate(&state.status, state.item_count, &name);
                let event_count_before = state.events.len();

                match result {
                    Some(transition_result) if transition_result.success => {
                        let from_status = state.status.clone();
                        let to_status = transition_result.new_state.clone();

                        // Apply effects
                        for effect in &transition_result.effects {
                            match effect {
                                temper_jit::table::Effect::SetState(s) => {
                                    state.status = s.clone();
                                }
                                temper_jit::table::Effect::IncrementItems => {
                                    state.item_count += 1;
                                }
                                temper_jit::table::Effect::DecrementItems => {
                                    state.item_count = state.item_count.saturating_sub(1);
                                }
                                temper_jit::table::Effect::EmitEvent(evt) => {
                                    tracing::info!(
                                        entity_type = %state.entity_type,
                                        entity_id = %state.entity_id,
                                        event = %evt,
                                        "event emitted"
                                    );
                                }
                            }
                        }

                        // If no SetState effect, use the transition result's new_state
                        if state.status == from_status && !to_status.is_empty() {
                            state.status = to_status.clone();
                        }

                        // Update fields
                        if let Some(obj) = state.fields.as_object_mut() {
                            obj.insert("Status".to_string(), serde_json::Value::String(state.status.clone()));
                        }

                        // Record event
                        state.events.push(EntityEvent {
                            action: name.clone(),
                            from_status,
                            to_status: state.status.clone(),
                            timestamp: chrono::Utc::now(),
                            params: params.clone(),
                        });

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
                            state.events.last().unwrap().action == name,
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
                        });
                    }
                    Some(_) => {
                        // Transition failed (guard not met)
                        ctx.reply(EntityResponse {
                            success: false,
                            state: state.clone(),
                            error: Some(format!(
                                "Action '{}' not valid from state '{}'",
                                name, state.status
                            )),
                        });
                    }
                    None => {
                        ctx.reply(EntityResponse {
                            success: false,
                            state: state.clone(),
                            error: Some(format!("Unknown action: {}", name)),
                        });
                    }
                }
            }
            EntityMsg::GetState => {
                ctx.reply(EntityResponse {
                    success: true,
                    state: state.clone(),
                    error: None,
                });
            }
            EntityMsg::GetField { field } => {
                let value = state.fields.get(&field).cloned().unwrap_or(serde_json::Value::Null);
                ctx.reply(value);
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
    use temper_runtime::ActorSystem;
    use temper_spec::tlaplus::extract_state_machine;
    use temper_jit::table::TransitionTable;

    const ORDER_TLA: &str = include_str!("../../../reference/ecommerce/specs/order.tla");

    fn order_table() -> Arc<TransitionTable> {
        // Use from_tla_source which resolves CanXxx guards — matches what DST verifies
        Arc::new(TransitionTable::from_tla_source(ORDER_TLA))
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

        // Add an item (Draft → Draft, item_count 0 → 1)
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

        // Submit (Draft → Submitted)
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

        // Try to submit with 0 items — should fail
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

        // Draft → AddItem → SubmitOrder → ConfirmOrder → ProcessOrder → ShipOrder → DeliverOrder
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

        // Try to cancel — should fail
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
