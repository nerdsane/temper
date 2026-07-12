//! SpecDrivenActor — implements the Actor trait backed by an IOA spec.
//!
//! Specs describe state machines (states, transitions, guards, effects).
//! The routing is external — reaction rules wire emit effects to target actors.
//!
//! # Architecture
//!
//! - Spec → TransitionTable (via temper-jit)
//! - Reaction rules → canonical reaction registry
//! - handle(): evaluate table → apply effects → route emits via ctx.tell()
//!
//! # Message protocol
//!
//! Actors communicate via `SpecMessage { action, params }`:
//! - `action`: the action/emit name (e.g., "PrepareContext")
//! - `params`: JSON-encoded params (empty for actions with no params)

use std::collections::BTreeMap;

use temper_jit::table::{EffectExecution, EffectState, TransitionTable};
use temper_runtime::reaction::{ReactionRegistry, ReactionRule, TargetResolver};
use temper_spec::automaton::Automaton;

use crate::actor::{
    Actor, ActorBudgets, ActorContext, ActorError, ActorHandle, BufferedTell, Message,
};

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
    /// Durable reaction cascade depth. External and scheduled actions start at zero.
    #[prost(uint32, tag = "3")]
    pub cascade_depth: u32,
}

impl SpecMessage {
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            params: Vec::new(),
            cascade_depth: 0,
        }
    }

    pub fn with_params(action: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            action: action.into(),
            params: serde_json::to_vec(&params).unwrap_or_default(),
            cascade_depth: 0,
        }
    }

    fn routed(action: impl Into<String>, cascade_depth: u32) -> Self {
        Self {
            action: action.into(),
            params: Vec::new(),
            cascade_depth,
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

impl EffectState for SpecActorState {
    fn status(&self) -> &str {
        &self.status
    }

    fn status_mut(&mut self) -> &mut String {
        &mut self.status
    }

    fn legacy_item_count(&self) -> Option<usize> {
        None
    }

    fn legacy_item_count_mut(&mut self) -> Option<&mut usize> {
        None
    }

    fn counters(&self) -> &BTreeMap<String, usize> {
        &self.counters
    }

    fn counters_mut(&mut self) -> &mut BTreeMap<String, usize> {
        &mut self.counters
    }

    fn booleans(&self) -> &BTreeMap<String, bool> {
        &self.booleans
    }

    fn booleans_mut(&mut self) -> &mut BTreeMap<String, bool> {
        &mut self.booleans
    }

    fn lists(&self) -> &BTreeMap<String, Vec<String>> {
        &self.lists
    }

    fn lists_mut(&mut self) -> &mut BTreeMap<String, Vec<String>> {
        &mut self.lists
    }

    fn fields(&self) -> &serde_json::Value {
        &self.fields
    }

    fn fields_mut(&mut self) -> &mut serde_json::Value {
        &mut self.fields
    }
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
    /// Canonical reaction rules, preserving fan-out and target semantics.
    reactions: ReactionRegistry,
    /// Leaked static refs for subscriptions() return.
    subscriptions_static: Vec<&'static str>,
    /// Exact conservative command bounds derived from the finite spec and registry.
    activation_budgets: ActorBudgets,
}

impl SpecDrivenActor {
    /// Create from an IOA TOML source and canonical reaction rules.
    pub fn from_ioa(ioa_source: &str, reactions: ReactionRegistry) -> Result<Self, String> {
        let automaton = temper_spec::parse_automaton(ioa_source)
            .map_err(|e| format!("failed to parse spec: {e}"))?;
        Ok(Self::from_automaton(&automaton, ioa_source, reactions))
    }

    /// Create from a pre-parsed automaton and reaction registry.
    pub fn from_automaton(
        automaton: &Automaton,
        ioa_source: &str,
        reactions: ReactionRegistry,
    ) -> Self {
        let name = automaton.automaton.name.clone();
        let table = TransitionTable::from_ioa_source(ioa_source);
        let activation_budgets = activation_budgets(&table, &reactions, &name);

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
            reactions,
            subscriptions_static,
            activation_budgets,
        }
    }

    /// Which message types this actor accepts.
    pub fn subscription_strings(&self) -> &[&'static str] {
        &self.subscriptions_static
    }

    /// Canonical reaction registry used by this actor.
    pub fn reactions(&self) -> &ReactionRegistry {
        &self.reactions
    }
}

#[async_trait::async_trait]
impl Actor for SpecDrivenActor {
    fn actor_type(&self) -> &str {
        &self.name
    }

    fn activation_budgets(&self) -> ActorBudgets {
        self.activation_budgets
    }

    fn initial_state(&self) -> Vec<u8> {
        serde_json::to_vec(&self.init_state).unwrap_or_default()
    }

    fn initial_state_with_fields(&self, fields: serde_json::Value) -> Result<Vec<u8>, ActorError> {
        let mut state = self.init_state.clone();
        state.fields = fields;
        serde_json::to_vec(&state)
            .map_err(|error| ActorError::HandlerFailed(format!("initial state ser: {error}")))
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
        let cascade_depth = spec_msg
            .as_ref()
            .map(|message| message.cascade_depth)
            .unwrap_or(0);

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

        let action_params = spec_msg
            .as_ref()
            .filter(|m| !m.params.is_empty())
            .and_then(|m| serde_json::from_slice::<serde_json::Value>(&m.params).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if !action_params
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
        {
            match (
                actor_state.fields.as_object_mut(),
                action_params.as_object(),
            ) {
                (Some(existing), Some(new_fields)) => {
                    for (k, v) in new_fields {
                        existing.insert(k.clone(), v.clone());
                    }
                }
                _ => actor_state.fields = action_params.clone(),
            }
        }

        let eval_ctx = temper_jit::table::build_effect_eval_context(&actor_state);

        // 2. Evaluate transition table.
        let result = self
            .table
            .evaluate_ctx(&actor_state.status, &eval_ctx, &action);

        match result {
            Some(r) if r.success => {
                let from_status = actor_state.status.clone();

                // 3. Apply effects through the shared exhaustive executor.
                let execution =
                    temper_jit::table::apply_effects(&mut actor_state, &r.effects, &action_params);

                // 4. Apply state transition fallback (if no SetState effect fired).
                if actor_state.status == from_status && !r.new_state.is_empty() {
                    actor_state.status = r.new_state.clone();
                }
                self.execute_commands(&actor_state, execution, cascade_depth, ctx)
                    .await?;

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

fn activation_budgets(
    table: &TransitionTable,
    reactions: &ReactionRegistry,
    actor_type: &str,
) -> ActorBudgets {
    let reaction_count = reactions.rules_for_actor_count(actor_type);
    let create_if_missing_count = reactions.create_if_missing_for_actor_count(actor_type);
    table
        .rules
        .iter()
        .map(|rule| {
            let routed_commands = rule
                .effects
                .iter()
                .filter(|effect| {
                    matches!(
                        effect,
                        temper_jit::table::Effect::EmitEvent(_)
                            | temper_jit::table::Effect::Custom(_)
                    )
                })
                .count();
            let scheduled_commands = rule
                .effects
                .iter()
                .filter(|effect| {
                    matches!(
                        effect,
                        temper_jit::table::Effect::ScheduleAction { .. }
                            | temper_jit::table::Effect::ScheduleAtAction { .. }
                    )
                })
                .count();
            let spawned_commands = rule
                .effects
                .iter()
                .filter(|effect| matches!(effect, temper_jit::table::Effect::SpawnEntity { .. }))
                .count();
            ActorBudgets {
                max_tells: routed_commands
                    .saturating_mul(reaction_count)
                    .saturating_add(scheduled_commands),
                max_spawns: routed_commands
                    .saturating_mul(create_if_missing_count)
                    .saturating_add(spawned_commands),
            }
        })
        .fold(
            ActorBudgets {
                max_tells: 0,
                max_spawns: 0,
            },
            |maximum, current| ActorBudgets {
                max_tells: maximum.max_tells.max(current.max_tells),
                max_spawns: maximum.max_spawns.max(current.max_spawns),
            },
        )
}

#[path = "spec_actor/commands.rs"]
mod commands;
#[cfg(test)]
#[path = "spec_actor/tests.rs"]
mod tests;
