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

use std::collections::{BTreeMap, HashMap};

use temper_jit::table::{EvalContext, TransitionTable};
use temper_runtime::reaction::ReactionRule;
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

// ─── Actor state ─────────────────────────────────────────────────────────────

/// Serializable state for spec-driven actors.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SpecActorState {
    pub status: String,
    #[serde(default)]
    pub counters: BTreeMap<String, usize>,
    #[serde(default)]
    pub booleans: BTreeMap<String, bool>,
    #[serde(default)]
    pub lists: BTreeMap<String, Vec<String>>,
    /// Arbitrary extra data — used to thread params through the reaction chain.
    /// SpecDrivenActor stores the last incoming message params here so integrations
    /// can read them from the trigger message.
    #[serde(default)]
    pub fields: serde_json::Value,
}

impl SpecActorState {
    fn to_eval_context(&self) -> EvalContext {
        let mut ctx = EvalContext::default();
        for (k, v) in &self.counters {
            ctx.counters.insert(k.clone(), *v);
        }
        for (k, v) in &self.booleans {
            ctx.booleans.insert(k.clone(), *v);
        }
        for (k, v) in &self.lists {
            ctx.lists.insert(k.clone(), v.clone());
        }
        ctx
    }
}

// ─── Routing map builder ─────────────────────────────────────────────────────

/// Build per-actor routing maps from reaction rules.
///
/// Returns `HashMap<actor_type, HashMap<emit_name, (target_actor_type, target_action)>>`.
pub fn build_routing_maps(
    rules: &[ReactionRule],
) -> HashMap<String, HashMap<String, (String, String)>> {
    let mut maps: HashMap<String, HashMap<String, (String, String)>> = HashMap::new();

    for rule in rules {
        if let Some(emit_name) = &rule.when.action {
            maps.entry(rule.when.entity_type.clone())
                .or_default()
                .insert(
                    emit_name.clone(),
                    (rule.then.entity_type.clone(), rule.then.action.clone()),
                );
        }
    }

    maps
}

/// Build a single actor's routing map from a reaction registry.
pub fn build_actor_routing(
    actor_type: &str,
    rules: &[ReactionRule],
) -> HashMap<String, (String, String)> {
    rules
        .iter()
        .filter(|r| r.when.entity_type == actor_type)
        .filter_map(|r| {
            r.when.action.as_ref().map(|emit| {
                (
                    emit.clone(),
                    (r.then.entity_type.clone(), r.then.action.clone()),
                )
            })
        })
        .collect()
}

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
        }
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
    fn actor_type(&self) -> &str {
        &self.name
    }

    fn initial_state(&self) -> Vec<u8> {
        serde_json::to_vec(&self.init_state).unwrap_or_default()
    }

    async fn handle(
        &self,
        ctx: &ActorContext,
        state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError> {
        // 1. Deserialize state.
        let mut actor_state: SpecActorState = if state.is_empty() {
            self.init_state.clone()
        } else {
            serde_json::from_slice(state)
                .map_err(|e| ActorError::HandlerFailed(format!("state deser: {e}")))?
        };

        // 2. Resolve action name + params.
        // If the message carries a SpecMessage, extract the action from its payload.
        // This handles both direct SpecMessage sends and raw action-name messages.
        let spec_msg = if message.message_type.ends_with("SpecMessage") {
            message.decode::<SpecMessage>().ok()
        } else {
            None
        };
        let action = spec_msg
            .as_ref()
            .filter(|m| !m.action.is_empty())
            .map(|m| m.action.clone())
            .unwrap_or_else(|| message.message_type.clone());

        let incoming_params = spec_msg
            .as_ref()
            .filter(|message| !message.params.is_empty())
            .and_then(|message| serde_json::from_slice::<serde_json::Value>(&message.params).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let incoming_params = self
            .table
            .canonicalize_action_params(&action, &incoming_params);
        self.table
            .validate_required_action_params(&action, &incoming_params)
            .map_err(|error| ActorError::HandlerFailed(error.to_string()))?;

        // Store incoming params in state.fields so integrations can read them.
        // Merge non-empty params into fields to preserve context from prior steps
        // (e.g. child Process keeps parent_pid while later messages add user_prompt/response).
        // For a new user turn, clear transient scratchpad fields from prior turns.
        if self.name == "Process"
            && matches!(action.as_str(), "StartProcess" | "SendInput")
            && let Some(obj) = actor_state.fields.as_object_mut()
        {
            for key in [
                "tool_calls",
                "tool_results",
                "child_result",
                "response",
                "error",
            ] {
                obj.remove(key);
            }
        }

        if let Some(fields) = (!incoming_params
            .as_object()
            .is_some_and(|object| object.is_empty()))
        .then_some(incoming_params)
        {
            match (actor_state.fields.as_object_mut(), fields.as_object()) {
                (Some(existing), Some(new_fields)) => {
                    for (k, v) in new_fields {
                        existing.insert(k.clone(), v.clone());
                    }
                }
                _ => actor_state.fields = fields,
            }
        }

        let eval_ctx = actor_state.to_eval_context();

        // 2. Evaluate transition table.
        let result = self
            .table
            .evaluate_ctx(&actor_state.status, &eval_ctx, &action);

        match result {
            Some(r) if r.success => {
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
            Some(_) => {
                tracing::warn!(
                    actor = %self.name,
                    action = %action,
                    status = %actor_state.status,
                    "action not valid from current state"
                );
            }
            None => {
                tracing::warn!(
                    actor = %self.name,
                    action = %action,
                    "unknown action"
                );
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
                        SpecMessage::with_params(target_action.clone(), state.fields.clone()),
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
                        SpecMessage::with_params(target_action.clone(), state.fields.clone()),
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
mod tests {
    use super::*;

    const SIMPLE_SPEC: &str = r#"
[automaton]
name = "TestActor"
states = ["Idle", "Running"]
initial = "Idle"

[[state]]
name = "rounds"
type = "counter"
initial = "0"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Running"
effect = [{ type = "increment", var = "rounds" }]

[[action]]
name = "Stop"
kind = "input"
from = ["Running"]
to = "Idle"
"#;

    #[test]
    fn test_spec_driven_actor_initial_state() {
        let actor = SpecDrivenActor::from_ioa(SIMPLE_SPEC, HashMap::new()).unwrap();
        let state_bytes = actor.initial_state();
        let state: SpecActorState = serde_json::from_slice(&state_bytes).unwrap();
        assert_eq!(state.status, "Idle");
        assert_eq!(state.counters.get("rounds"), Some(&0usize));
    }

    #[test]
    fn test_routing_map_builder() {
        let rules = vec![ReactionRule {
            name: "a".into(),
            when: temper_runtime::reaction::ReactionTrigger {
                entity_type: "Agent".into(),
                action: Some("PrepareContext".into()),
                to_state: None,
            },
            then: temper_runtime::reaction::ReactionTarget {
                entity_type: "ContextManager".into(),
                action: "PrepareContext".into(),
            },
            resolve_target: temper_runtime::reaction::TargetResolver::SameId,
        }];

        let maps = build_routing_maps(&rules);
        assert_eq!(maps["Agent"]["PrepareContext"].0, "ContextManager");
        assert_eq!(maps["Agent"]["PrepareContext"].1, "PrepareContext");
    }
}
