//! SpecDrivenActor — implements the Actor trait backed by an IOA spec.
//!
//! Specs describe state machines (states, transitions, guards, effects).
//! The routing is external — reaction rules wire emit effects to target actors.
//!
//! # Architecture
//!
//! - Spec → TransitionTable (via temper-jit)
//! - Reaction rules → routing map (emit name → target actor type)
//! - handle(): evaluate table → apply effects → route emits via ctx.tell()
//!
//! # Message protocol
//!
//! Actors communicate via `SpecMessage { action, params }`:
//! - `action`: the action/emit name (e.g., "PrepareContext")
//! - `params`: JSON-encoded params (empty for actions with no params)

use std::collections::HashMap;

use temper_jit::table::TransitionTable;
use temper_spec::automaton::Automaton;

use crate::actor::{Actor, ActorContext, ActorError, ActorHandle, Message};

// ─── SpecMessage ─────────────────────────────────────────────────────────────

/// Generic message for spec-driven actor communication.
/// The action name matches the IOA spec action/emit name.
#[derive(Clone, PartialEq, prost::Message)]
pub struct SpecMessage {
    /// The action/emit name (e.g., "PrepareContext", "ToolCallBatchRequested").
    #[prost(string, tag = "1")]
    pub action: String,
    /// JSON-encoded params (empty bytes for parameterless actions).
    #[prost(bytes, tag = "2")]
    pub params: Vec<u8>,
}

impl SpecMessage {
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            params: Vec::new(),
        }
    }

    pub fn with_params(action: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            action: action.into(),
            params: serde_json::to_vec(&params).unwrap_or_default(),
        }
    }
}

/// Internal reaction delivery. Source fields are projected at the receiving
/// actor, where its declared action inputs are available. External requests
/// cannot use this envelope without an actor sender.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct RoutedSpecMessage {
    // Keep the SpecMessage wire layout: concrete integration actors decode it.
    #[prost(string, tag = "1")]
    pub action: String,
    #[prost(bytes, tag = "2")]
    pub params: Vec<u8>,
}

impl From<SpecMessage> for RoutedSpecMessage {
    fn from(message: SpecMessage) -> Self {
        Self {
            action: message.action,
            params: message.params,
        }
    }
}

// ─── Actor state ─────────────────────────────────────────────────────────────

#[path = "spec_actor_state.rs"]
mod state;
pub use state::SpecActorState;

// ─── Routing map builder ─────────────────────────────────────────────────────

#[path = "spec_actor_routing.rs"]
mod routing;
pub use routing::{build_actor_routing, build_routing_maps};

// ─── SpecDrivenActor ─────────────────────────────────────────────────────────

/// An Actor implementation driven by an IOA spec + reaction routing.
///
/// - State machine transitions validated by the TransitionTable
/// - Emit effects routed to sibling actors via ctx.tell()
/// - Trigger effects sent to integration actors via ctx.tell()
pub struct SpecDrivenActor {
    /// Actor type name (e.g., "Agent", "ContextManager").
    name: String,
    /// TransitionTable compiled from the IOA spec.
    table: TransitionTable,
    /// Initial state (from spec's initial state + variable declarations).
    init_state: SpecActorState,
    /// Routing map: emit/trigger name → (target actor type, target action).
    routing: HashMap<String, (String, String)>,
    /// Leaked static refs for subscriptions() return.
    subscriptions_static: Vec<&'static str>,
    /// Application-owned fields cleared only by an accepted configured action.
    input_field_resets: HashMap<String, Vec<String>>,
}

fn has_input_contracts(table: &TransitionTable) -> bool {
    table.strict_action_params
        || table
            .action_contracts
            .values()
            .any(|contract| !contract.constraints.is_empty())
}

impl SpecDrivenActor {
    /// Create from an IOA TOML source + routing map.
    pub fn from_ioa(
        ioa_source: &str,
        routing: HashMap<String, (String, String)>,
    ) -> Result<Self, String> {
        let automaton = temper_spec::parse_automaton(ioa_source)
            .map_err(|e| format!("failed to parse spec: {e}"))?;
        Ok(Self::from_automaton(&automaton, ioa_source, routing))
    }

    /// Create from a pre-parsed Automaton + routing map.
    pub fn from_automaton(
        automaton: &Automaton,
        ioa_source: &str,
        routing: HashMap<String, (String, String)>,
    ) -> Self {
        let name = automaton.automaton.name.clone();
        let table = TransitionTable::from_ioa_source(ioa_source);

        // Build initial state from spec variables.
        let mut init_state = SpecActorState {
            status: automaton.automaton.initial.clone(),
            ..Default::default()
        };
        for var in &automaton.state {
            match var.var_type.as_str() {
                "counter" => {
                    let v: usize = var.initial.parse().unwrap_or(0);
                    init_state.counters.insert(var.name.clone(), v);
                }
                "bool" => {
                    let v: bool = var.initial.parse().unwrap_or(false);
                    init_state.booleans.insert(var.name.clone(), v);
                }
                "list" | "set" => {
                    init_state.lists.insert(var.name.clone(), Vec::new());
                }
                _ => {}
            }
        }

        table.initialize_declared_fields(
            &mut init_state.fields,
            &mut init_state.counters,
            &mut init_state.booleans,
        );
        if has_input_contracts(&table) {
            init_state.lists = table.initial_values.lists.clone();
        }

        // Input actions are the message types this actor accepts.
        // NOTE: Box::leak is intentional — actors are singletons, never dropped.
        let subscriptions_static: Vec<&'static str> = automaton
            .actions
            .iter()
            .filter(|a| a.kind == "input")
            .map(|a| &*Box::leak(a.name.clone().into_boxed_str()))
            .collect();

        Self {
            name,
            table,
            init_state,
            routing,
            subscriptions_static,
            input_field_resets: HashMap::new(),
        }
    }

    /// Configure application fields cleared after an action is accepted.
    pub fn with_input_field_resets(mut self, resets: HashMap<String, Vec<String>>) -> Self {
        self.input_field_resets = resets;
        self
    }

    /// Which message types this actor accepts.
    pub fn subscription_strings(&self) -> &[&'static str] {
        &self.subscriptions_static
    }

    /// The routing map (emit name → target actor type).
    pub fn routing(&self) -> &HashMap<String, (String, String)> {
        &self.routing
    }
}

#[async_trait::async_trait]
impl Actor for SpecDrivenActor {
    fn validate_initial_fields(&self, fields: &serde_json::Value) -> Result<(), ActorError> {
        self.table
            .validate_initial_fields(fields)
            .map_err(ActorError::Rejected)
    }

    fn actor_type(&self) -> &str {
        &self.name
    }

    fn initial_state(&self) -> Vec<u8> {
        serde_json::to_vec(&self.init_state).unwrap_or_default()
    }

    fn initial_state_for(&self, handle: &ActorHandle) -> Vec<u8> {
        if !has_input_contracts(&self.table) {
            return self.initial_state();
        }
        let mut state = self.init_state.clone();
        if state.fields.is_null() {
            state.fields = serde_json::json!({});
        }
        if let Some(fields) = state.fields.as_object_mut() {
            fields.insert("Id".into(), serde_json::json!(handle.namespace));
            fields.insert("id".into(), serde_json::json!(handle.namespace));
        }
        serde_json::to_vec(&state).unwrap_or_default()
    }

    async fn handle(
        &self,
        ctx: &ActorContext,
        state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        // Supported creation persists initial bytes before the actor accepts messages.
        if state.is_empty() && has_input_contracts(&self.table) {
            return Err(ActorError::Rejected(
                "contracted actor has no persisted initial state".into(),
            ));
        }
        let mut actor_state: SpecActorState = if state.is_empty() {
            self.init_state.clone()
        } else {
            serde_json::from_slice(state)
                .map_err(|e| ActorError::HandlerFailed(format!("state deser: {e}")))?
        };

        // Decode strict inputs at the boundary, before touching actor state.
        let routed = message.message_type == "RoutedSpecMessage";
        if routed && message.from.is_none() {
            return Err(ActorError::Rejected(
                "routed action requires an actor sender".into(),
            ));
        }
        let spec_msg = if message.message_type.ends_with("SpecMessage") {
            match message.decode::<SpecMessage>() {
                Ok(message) => Some(message),
                Err(error) if self.table.strict_action_params => {
                    return Err(ActorError::Rejected(format!(
                        "invalid SpecMessage: {error}"
                    )));
                }
                Err(_) => None,
            }
        } else {
            None
        };
        let action = spec_msg
            .as_ref()
            .filter(|message| !message.action.is_empty())
            .map(|message| message.action.as_str())
            .unwrap_or(&message.message_type);
        let validates_input = self.table.strict_action_params
            || self
                .table
                .action_contracts
                .get(action)
                .is_some_and(|contract| !contract.constraints.is_empty());
        let params_bytes = spec_msg
            .as_ref()
            .map(|message| message.params.as_slice())
            .or_else(|| validates_input.then_some(message.payload.as_slice()));
        let mut params = match params_bytes.filter(|bytes| !bytes.is_empty()) {
            Some(bytes) => match serde_json::from_slice::<serde_json::Value>(bytes) {
                Ok(value) => value,
                Err(error) if validates_input => {
                    return Err(ActorError::Rejected(format!(
                        "invalid action JSON: {error}"
                    )));
                }
                Err(_) => serde_json::json!({}),
            },
            None => serde_json::json!({}),
        };
        if routed && self.table.strict_action_params {
            if params.is_null() {
                params = serde_json::json!({});
            }
            let contract = self.table.action_contracts.get(action).ok_or_else(|| {
                ActorError::Rejected(format!(
                    "Action '{action}' has no declared parameter contract"
                ))
            })?;
            let fields = params.as_object_mut().ok_or_else(|| {
                ActorError::Rejected("routed source fields must be a JSON object".into())
            })?;
            fields.retain(|name, _| contract.params.contains(name));
        }
        self.table
            .validate_action_params(
                action,
                &params,
                &actor_state.fields,
                &actor_state.counters,
                &actor_state.booleans,
            )
            .map_err(ActorError::Rejected)?;
        let eval_ctx = actor_state.to_eval_context();
        let result = self
            .table
            .evaluate_ctx(&actor_state.status, &eval_ctx, action);

        match result {
            Some(r) if r.success => {
                if let Some(reset_fields) = self.input_field_resets.get(action)
                    && let Some(fields) = actor_state.fields.as_object_mut()
                {
                    for key in reset_fields {
                        fields.remove(key);
                    }
                }
                if !params.as_object().is_some_and(|object| object.is_empty()) {
                    match (actor_state.fields.as_object_mut(), params.as_object()) {
                        (Some(existing), Some(incoming)) => existing.extend(incoming.clone()),
                        _ => actor_state.fields = params,
                    }
                }
                let from_status = actor_state.status.clone();

                // 3. Apply effects — may include SetState.
                for effect in &r.effects {
                    self.apply_effect(&mut actor_state, effect, ctx).await;
                }

                // 4. Apply state transition fallback (if no SetState effect fired).
                if actor_state.status == from_status && !r.new_state.is_empty() {
                    actor_state.status = r.new_state.clone();
                }

                tracing::info!(
                    actor = %self.name,
                    action = %action,
                    new_state = %actor_state.status,
                    "transition"
                );
            }
            denied => {
                let reason = if denied.is_some() {
                    "action not valid from current state"
                } else {
                    "unknown action"
                };
                tracing::warn!(actor = %self.name, action = %action, status = %actor_state.status, reason);
                return if self.table.strict_action_params {
                    Err(ActorError::Rejected(reason.into()))
                } else {
                    Ok(())
                };
            }
        }

        // 5. Serialize state back.
        *state = serde_json::to_vec(&actor_state)
            .map_err(|e| ActorError::HandlerFailed(format!("state ser: {e}")))?;

        Ok(())
    }
}

impl SpecDrivenActor {
    async fn apply_effect(
        &self,
        state: &mut SpecActorState,
        effect: &temper_jit::table::Effect,
        ctx: &ActorContext,
    ) {
        match effect {
            temper_jit::table::Effect::SetState(s) => {
                state.status = s.clone();
            }
            temper_jit::table::Effect::IncrementItems => {
                *state.counters.entry("items".into()).or_default() += 1;
            }
            temper_jit::table::Effect::IncrementCounter(var) => {
                *state.counters.entry(var.clone()).or_default() += 1;
            }
            temper_jit::table::Effect::DecrementItems => {
                let c = state.counters.entry("items".into()).or_default();
                *c = c.saturating_sub(1);
            }
            temper_jit::table::Effect::DecrementCounter(var) => {
                let c = state.counters.entry(var.clone()).or_default();
                *c = c.saturating_sub(1);
            }
            temper_jit::table::Effect::SetBool { var, value } => {
                state.booleans.insert(var.clone(), *value);
            }
            temper_jit::table::Effect::EmitEvent(emit_name) => {
                if let Some((target_type, target_action)) = self.routing.get(emit_name.as_str()) {
                    tracing::info!(actor=%self.name, emit=%emit_name, target=%target_type, target_action=%target_action, "routing emit");
                    let target =
                        ActorHandle::new(ctx.self_handle().namespace.clone(), target_type.clone());
                    ctx.tell(
                        &target,
                        RoutedSpecMessage::from(SpecMessage::with_params(
                            target_action.clone(),
                            state.fields.clone(),
                        )),
                    )
                    .await;
                } else {
                    tracing::warn!(
                        actor = %self.name,
                        emit = %emit_name,
                        "no routing for emit (no reaction rule)"
                    );
                }
            }
            temper_jit::table::Effect::Custom(trigger_name) => {
                if let Some((target_type, target_action)) = self.routing.get(trigger_name.as_str())
                {
                    tracing::info!(actor=%self.name, trigger=%trigger_name, target=%target_type, target_action=%target_action, "routing trigger");
                    let target =
                        ActorHandle::new(ctx.self_handle().namespace.clone(), target_type.clone());
                    ctx.tell(
                        &target,
                        RoutedSpecMessage::from(SpecMessage::with_params(
                            target_action.clone(),
                            state.fields.clone(),
                        )),
                    )
                    .await;
                } else {
                    tracing::warn!(
                        actor = %self.name,
                        trigger = %trigger_name,
                        "no routing for trigger"
                    );
                }
            }
            _ => {
                tracing::debug!("unhandled effect: {:?}", effect);
            }
        }
    }
}

#[cfg(test)]
#[path = "spec_actor_strict_tests.rs"]
mod strict_tests;
