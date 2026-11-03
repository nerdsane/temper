//! SpecDrivenActor — implements the Actor trait backed by an IOA spec.
//!
//! Specs describe state machines (states, transitions, guards, effects) and
//! their outgoing calls: entity-kind `[[action.triggers]]` route an accepted
//! action to sibling actors.
//!
//! # Architecture
//!
//! - Spec → TransitionTable (via temper-jit)
//! - Entity triggers → routing map (action name → target actor types)
//! - handle(): evaluate table → apply effects → route via ctx.tell()
//!
//! # Message protocol
//!
//! Actors communicate via `SpecMessage { action, params }`:
//! - `action`: the action/emit name (e.g., "PrepareContext")
//! - `params`: JSON-encoded params (empty for actions with no params)

use std::collections::{BTreeMap, HashMap};

use temper_jit::table::{TransitionTable, effect_args};
use temper_spec::automaton::Automaton;

use crate::actor::{Actor, ActorContext, ActorError, ActorHandle, Message};

// ─── SpecMessage ─────────────────────────────────────────────────────────────

/// Generic message for spec-driven actor communication.
/// The action name matches the IOA spec action name.
#[derive(Clone, PartialEq, prost::Message)]
pub struct SpecMessage {
    /// The action name (e.g., "PrepareContext", "ToolCallBatchRequested").
    #[prost(string, tag = "1")]
    pub action: String,
    /// JSON-encoded params (empty bytes for parameterless actions).
    #[prost(bytes, tag = "2")]
    pub params: Vec<u8>,
    /// Persisted row version used to authorize an external action, when present.
    /// The PostgreSQL activator checks it before invoking any actor handler.
    #[prost(int64, optional, tag = "3")]
    pub expected_state_version: Option<i64>,
}

impl SpecMessage {
    /// Bind execution to the persisted state that authorized this message.
    pub fn if_state_version(mut self, version: i64) -> Self {
        self.expected_state_version = Some(version);
        self
    }

    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            params: Vec::new(),
            expected_state_version: None,
        }
    }

    pub fn with_params(action: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            action: action.into(),
            params: serde_json::to_vec(&params).unwrap_or_default(),
            expected_state_version: None,
        }
    }
}

/// Internal reaction delivery. Source fields are projected at the receiving
/// actor, where its declared action inputs are available. HTTP requests use ordinary SpecMessage; this internal
/// envelope requires a sender but does not authenticate in-process callers.
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
pub use routing::build_actor_routing;

// ─── SpecDrivenActor ─────────────────────────────────────────────────────────

/// An Actor implementation driven by an IOA spec.
///
/// - State machine transitions validated by the TransitionTable
/// - Accepted actions routed to sibling and integration actors via
///   ctx.tell(), per the spec's entity triggers
pub struct SpecDrivenActor {
    /// Actor type name (e.g., "Agent", "ContextManager").
    name: String,
    /// TransitionTable compiled from the IOA spec.
    table: TransitionTable,
    /// Initial state (from spec's initial state + variable declarations).
    init_state: SpecActorState,
    /// Routing map: action name → (target actor type, target action) per
    /// entity trigger.
    routing: BTreeMap<String, Vec<(String, String)>>,
    /// Leaked static refs for subscriptions() return.
    subscriptions_static: Vec<&'static str>,
    /// Application-owned fields cleared only by an accepted configured action.
    input_field_resets: HashMap<String, Vec<String>>,
}

impl SpecDrivenActor {
    /// Create from an IOA TOML source.
    pub fn from_ioa(ioa_source: &str) -> Result<Self, String> {
        let automaton = temper_spec::parse_automaton(ioa_source)
            .map_err(|e| format!("failed to parse spec: {e}"))?;
        Ok(Self::from_automaton(&automaton))
    }

    /// Create from a pre-parsed Automaton.
    pub fn from_automaton(automaton: &Automaton) -> Self {
        let name = automaton.automaton.name.clone();
        let routing = build_actor_routing(automaton);
        let table = TransitionTable::from_automaton(automaton);

        // Build initial state from spec variables.
        let mut init_state = SpecActorState {
            status: automaton.automaton.initial.clone(),
            ..Default::default()
        };
        for var in &automaton.state {
            use temper_spec::automaton::Initial;
            match &var.initial {
                Initial::Counter(n) => {
                    init_state.counters.insert(var.name.clone(), *n);
                }
                Initial::Bool(b) => {
                    init_state.booleans.insert(var.name.clone(), *b);
                }
                Initial::List(items) => {
                    init_state.lists.insert(var.name.clone(), items.clone());
                }
                Initial::String(_) | Initial::Int(_) => {}
            }
        }

        table.initialize_declared_fields(
            &mut init_state.fields,
            &mut init_state.counters,
            &mut init_state.booleans,
        );
        if table.has_input_contracts() {
            init_state.lists = table.initial_values.lists.clone();
        }

        // Input actions are the message types this actor accepts.
        // NOTE: Box::leak is intentional — actors are singletons, never dropped.
        let subscriptions_static: Vec<&'static str> = automaton
            .actions
            .iter()
            .filter(|a| a.kind == temper_spec::automaton::ActionKind::Input)
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

    /// The routing map (action name → target actor types and actions).
    pub fn routing(&self) -> &BTreeMap<String, Vec<(String, String)>> {
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
        if !self.table.has_input_contracts() {
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
        if state.is_empty() && self.table.has_input_contracts() {
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
            .map_err(|error| ActorError::Rejected(error.message))?;
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
                // Legacy actions can pass through a non-object payload. Keep
                // that behavior before effects materialize any string fields.
                if !params.is_object() {
                    actor_state.fields = params.clone();
                }
                let from_status = actor_state.status.clone();

                // 3. Apply effects — may include SetState.
                for effect in &r.effects {
                    self.apply_effect(&mut actor_state, effect, &params);
                }

                // Project fields after effects, matching the server and model:
                // a parameter wins when it writes the same string as an effect.
                if let Some(incoming) = params.as_object()
                    && !incoming.is_empty()
                {
                    match actor_state.fields.as_object_mut() {
                        Some(existing) => existing.extend(incoming.clone()),
                        None => actor_state.fields = params,
                    }
                }

                // 4. Apply state transition fallback (if no SetState effect fired).
                if actor_state.status == from_status && !r.new_state.is_empty() {
                    actor_state.status = r.new_state.clone();
                }

                // 5. Route the accepted action per the spec's entity triggers.
                for (target_type, target_action) in self.routing.get(action).into_iter().flatten() {
                    tracing::info!(actor=%self.name, action=%action, target=%target_type, target_action=%target_action, "routing action");
                    let target =
                        ActorHandle::new(ctx.self_handle().namespace.clone(), target_type.clone());
                    ctx.tell(
                        &target,
                        RoutedSpecMessage::from(SpecMessage::with_params(
                            target_action.clone(),
                            actor_state.fields.clone(),
                        )),
                    )
                    .await;
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
    fn apply_effect(
        &self,
        state: &mut SpecActorState,
        effect: &temper_jit::table::Effect,
        params: &serde_json::Value,
    ) {
        use temper_jit::table::Effect;
        let declared = &self.table.initial_values;
        match effect {
            Effect::SetState(s) => {
                state.status = s.clone();
            }
            Effect::SetCounter { var, value } => {
                if let Some(value) = effect_args::count(value, &state.counters, declared, params) {
                    state.counters.insert(var.clone(), value);
                }
            }
            Effect::AddCounter { var, value } => {
                let delta =
                    effect_args::count(value, &state.counters, declared, params).unwrap_or(0);
                let current = declared.counter(&state.counters, var).unwrap_or(0);
                state
                    .counters
                    .insert(var.clone(), current.saturating_add(delta));
            }
            Effect::SubCounter { var, value } => {
                let delta =
                    effect_args::count(value, &state.counters, declared, params).unwrap_or(0);
                let current = declared.counter(&state.counters, var).unwrap_or(0);
                state
                    .counters
                    .insert(var.clone(), current.saturating_sub(delta));
            }
            Effect::SetBool { var, value } => {
                if let Some(value) = effect_args::boolean(value, &state.booleans, declared, params)
                {
                    state.booleans.insert(var.clone(), value);
                }
            }
            Effect::SetString { var, value } => {
                // Legacy non-strict actions accept non-object params and may
                // replace `fields` with them before effects run.
                if !state.fields.is_object() {
                    state.fields = serde_json::json!({});
                }
                state.fields[var] = serde_json::Value::String(value.clone());
            }
            Effect::ListAppend { var, value } => {
                if let Some(element) = effect_args::string(value, params) {
                    declared.list_mut(&mut state.lists, var).push(element);
                }
            }
            Effect::ListRemoveAt { var, index } => {
                if let Some(index) = effect_args::count(index, &state.counters, declared, params) {
                    let list = declared.list_mut(&mut state.lists, var);
                    if index < list.len() {
                        list.remove(index);
                    }
                }
            }
            Effect::ListRemove { var, value } => {
                if let Some(element) = effect_args::string(value, params) {
                    declared
                        .list_mut(&mut state.lists, var)
                        .retain(|item| *item != element);
                }
            }
            _ => {
                tracing::debug!(actor = %self.name, "unhandled effect: {:?}", effect);
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/spec_actor_strict.rs"]
mod strict_tests;

#[cfg(test)]
#[path = "tests/spec_actor_string_effect.rs"]
mod string_effect_tests;
