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
    ///
    /// Fails closed (ADR-0168) if the compiled transition table uses effects
    /// this backend cannot execute (`schedule`, `schedule_at`, `spawn`).
    pub fn from_ioa(
        ioa_source: &str,
        routing: HashMap<String, (String, String)>,
    ) -> Result<Self, String> {
        let automaton = temper_spec::parse_automaton(ioa_source)
            .map_err(|e| format!("failed to parse spec: {e}"))?;
        Self::from_automaton(&automaton, ioa_source, routing)
    }

    /// Create from a pre-parsed Automaton + routing map.
    ///
    /// Returns `Err` when the table contains schedule/spawn effects that the
    /// PG actor-runtime cannot honor (ADR-0168 / ARN-179).
    pub fn from_automaton(
        automaton: &Automaton,
        ioa_source: &str,
        routing: HashMap<String, (String, String)>,
    ) -> Result<Self, String> {
        let name = automaton.automaton.name.clone();
        let table = TransitionTable::from_ioa_source(ioa_source);
        reject_unsupported_effects(&table)?;

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

        Ok(Self {
            name,
            table,
            init_state,
            routing,
            subscriptions_static,
        })
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

        if let Some(fields) = spec_msg
            .as_ref()
            .filter(|m| !m.params.is_empty())
            .and_then(|m| serde_json::from_slice::<serde_json::Value>(&m.params).ok())
            .filter(|p| !p.as_object().is_some_and(|o| o.is_empty()))
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

/// Reject schedule/spawn effects this backend cannot execute (ADR-0168).
fn reject_unsupported_effects(table: &TransitionTable) -> Result<(), String> {
    use temper_jit::table::Effect;
    for rule in &table.rules {
        for effect in &rule.effects {
            match effect {
                Effect::ScheduleAction { .. } => {
                    return Err(format!(
                        "unsupported effect 'schedule' on action '{}': \
                         PG actor-runtime has no timer pipeline (ADR-0168 / ARN-179)",
                        rule.name
                    ));
                }
                Effect::ScheduleAtAction { .. } => {
                    return Err(format!(
                        "unsupported effect 'schedule_at' on action '{}': \
                         PG actor-runtime has no timer pipeline (ADR-0168 / ARN-179)",
                        rule.name
                    ));
                }
                Effect::SpawnEntity { .. } => {
                    return Err(format!(
                        "unsupported effect 'spawn' on action '{}': \
                         PG actor-runtime has no spawn pipeline (ADR-0168 / ARN-179)",
                        rule.name
                    ));
                }
                // All other effects are either implemented or legacy no-ops.
                Effect::SetState(_)
                | Effect::IncrementItems
                | Effect::DecrementItems
                | Effect::IncrementCounter(_)
                | Effect::IncrementCounterByParam { .. }
                | Effect::DecrementCounter(_)
                | Effect::DecrementCounterByParam { .. }
                | Effect::SetCounterFromParam { .. }
                | Effect::SetBool { .. }
                | Effect::EmitEvent(_)
                | Effect::ListAppend(_)
                | Effect::ListRemoveAt(_)
                | Effect::Custom(_) => {}
            }
        }
    }
    Ok(())
}

fn counter_delta_from_params(params: &serde_json::Value, param: &str) -> usize {
    params
        .get(param)
        .and_then(|value| match value {
            serde_json::Value::Number(number) => number.as_u64().map(|v| v as usize),
            serde_json::Value::String(text) => text.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

impl SpecDrivenActor {
    /// Apply a compiled JIT effect to durable actor state (ADR-0168).
    ///
    /// Action params live in `state.fields` after the handle() merge step.
    /// Exhaustive match — no silent catch-all.
    async fn apply_effect(
        &self,
        state: &mut SpecActorState,
        effect: &temper_jit::table::Effect,
        ctx: &ActorContext,
    ) {
        use temper_jit::table::Effect;
        match effect {
            Effect::SetState(s) => {
                state.status = s.clone();
            }
            Effect::IncrementItems => {
                *state.counters.entry("items".into()).or_default() += 1;
            }
            Effect::IncrementCounter(var) => {
                *state.counters.entry(var.clone()).or_default() += 1;
            }
            Effect::IncrementCounterByParam { var, param } => {
                let delta = counter_delta_from_params(&state.fields, param);
                *state.counters.entry(var.clone()).or_default() += delta;
            }
            Effect::DecrementItems => {
                let c = state.counters.entry("items".into()).or_default();
                *c = c.saturating_sub(1);
            }
            Effect::DecrementCounter(var) => {
                let c = state.counters.entry(var.clone()).or_default();
                *c = c.saturating_sub(1);
            }
            Effect::DecrementCounterByParam { var, param } => {
                let delta = counter_delta_from_params(&state.fields, param);
                let c = state.counters.entry(var.clone()).or_default();
                *c = c.saturating_sub(delta);
            }
            Effect::SetCounterFromParam { var, param } => {
                let parsed = state.fields.get(param).and_then(|v| match v {
                    serde_json::Value::Number(n) => n.as_u64().map(|u| u as usize),
                    serde_json::Value::String(t) => t.parse::<usize>().ok(),
                    _ => None,
                });
                match parsed {
                    Some(value) => {
                        state.counters.insert(var.clone(), value);
                    }
                    None => tracing::warn!(
                        actor = %self.name,
                        counter = %var,
                        param = %param,
                        "set_counter_from_param skipped: param missing or not a non-negative integer"
                    ),
                }
            }
            Effect::SetBool { var, value } => {
                state.booleans.insert(var.clone(), *value);
            }
            Effect::ListAppend(var) => {
                // Same semantics as entity_actor: value is params[var] as string.
                if let Some(val) = state.fields.get(var).and_then(|v| v.as_str()) {
                    state
                        .lists
                        .entry(var.clone())
                        .or_default()
                        .push(val.to_string());
                } else {
                    tracing::warn!(
                        actor = %self.name,
                        list = %var,
                        "list_append skipped: param missing or not a string"
                    );
                }
            }
            Effect::ListRemoveAt(var) => {
                let index_key = format!("{var}_index");
                if let Some(idx) = state.fields.get(&index_key).and_then(|v| v.as_u64()) {
                    let list = state.lists.entry(var.clone()).or_default();
                    let idx = idx as usize;
                    if idx < list.len() {
                        list.remove(idx);
                    }
                } else {
                    tracing::warn!(
                        actor = %self.name,
                        list = %var,
                        index_key = %index_key,
                        "list_remove_at skipped: index param missing"
                    );
                }
            }
            Effect::EmitEvent(emit_name) => {
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
            Effect::Custom(trigger_name) => {
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
            // Construction rejects these (reject_unsupported_effects).
            // Fail fast if a table somehow bypasses the constructor (ADR-0168).
            Effect::ScheduleAction { .. }
            | Effect::ScheduleAtAction { .. }
            | Effect::SpawnEntity { .. } => {
                unreachable!(
                    "schedule/spawn effect reached apply_effect after construction rejection \
                     (ADR-0168 / ARN-179); actor={}",
                    self.name
                );
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

    // ─── ARN-179 durability regressions ───────────────────────────────────

    const LIST_SPEC: &str = r#"
[automaton]
name = "ListActor"
states = ["Idle", "Active"]
initial = "Idle"

[[state]]
name = "tags"
type = "list"
initial = "[]"

[[action]]
name = "AddTag"
kind = "input"
from = ["Idle", "Active"]
to = "Active"
effect = [{ type = "list_append", var = "tags" }]
"#;

    const COUNTER_PARAM_SPEC: &str = r#"
[automaton]
name = "CounterActor"
states = ["Idle", "Active"]
initial = "Idle"

[[state]]
name = "score"
type = "counter"
initial = "0"

[[action]]
name = "SetScore"
kind = "input"
from = ["Idle", "Active"]
to = "Active"
effect = [{ type = "set_counter_from_param", var = "score", param = "score" }]
"#;

    const SCHEDULE_SPEC: &str = r#"
[automaton]
name = "TimerActor"
states = ["Idle", "Waiting"]
initial = "Idle"

[[action]]
name = "Arm"
kind = "input"
from = ["Idle"]
to = "Waiting"
effect = [{ type = "schedule", action = "Fire", delay_seconds = 5 }]

[[action]]
name = "Fire"
kind = "input"
from = ["Waiting"]
to = "Idle"
"#;

    const SCHEDULE_AT_SPEC: &str = r#"
[automaton]
name = "TimerAtActor"
states = ["Idle", "Waiting"]
initial = "Idle"

[[action]]
name = "Arm"
kind = "input"
from = ["Idle"]
to = "Waiting"
effect = [{ type = "schedule_at", action = "Fire", field = "due_at" }]

[[action]]
name = "Fire"
kind = "input"
from = ["Waiting"]
to = "Idle"
"#;

    const SPAWN_SPEC: &str = r#"
[automaton]
name = "ParentActor"
states = ["Idle", "Spawned"]
initial = "Idle"

[[action]]
name = "CreateChild"
kind = "input"
from = ["Idle"]
to = "Spawned"
effect = [{ type = "spawn", entity_type = "Child", entity_id_source = "{uuid}" }]
"#;

    const COUNTER_DELTA_SPEC: &str = r#"
[automaton]
name = "DeltaActor"
states = ["Idle", "Active"]
initial = "Idle"

[[state]]
name = "score"
type = "counter"
initial = "10"

[[action]]
name = "Bump"
kind = "input"
from = ["Idle", "Active"]
to = "Active"
effect = [
  { type = "increment", var = "score", amount = "delta" },
  { type = "decrement", var = "score", amount = "penalty" },
]
"#;

    const LIST_REMOVE_SPEC: &str = r#"
[automaton]
name = "ListRemoveActor"
states = ["Idle", "Active"]
initial = "Idle"

[[state]]
name = "tags"
type = "list"
initial = "[]"

[[action]]
name = "AddTag"
kind = "input"
from = ["Idle", "Active"]
to = "Active"
effect = [{ type = "list_append", var = "tags" }]

[[action]]
name = "DropTag"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{ type = "list_remove_at", var = "tags" }]
"#;

    fn test_message(action: &str, params: serde_json::Value) -> Message {
        use prost::Message as _;
        let payload = SpecMessage::with_params(action, params);
        Message {
            id: 1,
            from: None,
            to: ActorHandle::new("test-ns", "TestActor"),
            message_type: "SpecMessage".into(),
            payload: payload.encode_to_vec(),
            correlation_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn test_ctx(actor_type: &str) -> ActorContext {
        ActorContext::new(ActorHandle::new("test-ns", actor_type), None, None)
    }

    /// RED: ListAppend was silently dropped; durable list state stayed empty.
    #[tokio::test]
    async fn list_append_effect_persists_into_actor_state() {
        let actor = SpecDrivenActor::from_ioa(LIST_SPEC, HashMap::new()).expect("parse");
        let ctx = test_ctx("ListActor");
        let mut state = actor.initial_state();
        let msg = test_message("AddTag", serde_json::json!({"tags": "alpha"}));
        actor.handle(&ctx, &mut state, &msg).await.expect("handle");
        let s: SpecActorState = serde_json::from_slice(&state).expect("deser");
        let tags = s.lists.get("tags").cloned().unwrap_or_default();
        assert_eq!(
            tags,
            vec!["alpha".to_string()],
            "ListAppend must append the param value into durable list state (ARN-179)"
        );
        assert_eq!(s.status, "Active");
    }

    /// RED: SetCounterFromParam was silently dropped; counter stayed at 0.
    #[tokio::test]
    async fn set_counter_from_param_persists_into_actor_state() {
        let actor = SpecDrivenActor::from_ioa(COUNTER_PARAM_SPEC, HashMap::new()).expect("parse");
        let ctx = test_ctx("CounterActor");
        let mut state = actor.initial_state();
        let msg = test_message("SetScore", serde_json::json!({"score": 42}));
        actor.handle(&ctx, &mut state, &msg).await.expect("handle");
        let s: SpecActorState = serde_json::from_slice(&state).expect("deser");
        assert_eq!(
            s.counters.get("score"),
            Some(&42usize),
            "SetCounterFromParam must write the param into durable counter state (ARN-179)"
        );
    }

    /// Schedule effects must not load silently — reject at construction.
    #[test]
    fn schedule_effect_rejected_at_construction() {
        match SpecDrivenActor::from_ioa(SCHEDULE_SPEC, HashMap::new()) {
            Ok(_) => panic!("schedule effect must fail closed at construction"),
            Err(err) => assert!(
                err.contains("schedule") || err.contains("unsupported"),
                "error must name the unsupported schedule effect, got: {err}"
            ),
        }
    }

    #[test]
    fn schedule_at_effect_rejected_at_construction() {
        match SpecDrivenActor::from_ioa(SCHEDULE_AT_SPEC, HashMap::new()) {
            Ok(_) => panic!("schedule_at effect must fail closed at construction"),
            Err(err) => assert!(
                err.contains("schedule_at") || err.contains("unsupported"),
                "error must name the unsupported schedule_at effect, got: {err}"
            ),
        }
    }

    #[test]
    fn spawn_effect_rejected_at_construction() {
        match SpecDrivenActor::from_ioa(SPAWN_SPEC, HashMap::new()) {
            Ok(_) => panic!("spawn effect must fail closed at construction"),
            Err(err) => assert!(
                err.contains("spawn") || err.contains("unsupported"),
                "error must name the unsupported spawn effect, got: {err}"
            ),
        }
    }

    #[tokio::test]
    async fn counter_by_param_deltas_apply() {
        let actor = SpecDrivenActor::from_ioa(COUNTER_DELTA_SPEC, HashMap::new()).expect("parse");
        let ctx = test_ctx("DeltaActor");
        let mut state = actor.initial_state();
        // start 10; +5; -3 => 12
        let msg = test_message("Bump", serde_json::json!({"delta": 5, "penalty": 3}));
        actor.handle(&ctx, &mut state, &msg).await.expect("handle");
        let s: SpecActorState = serde_json::from_slice(&state).expect("deser");
        assert_eq!(s.counters.get("score"), Some(&12usize));
    }

    #[tokio::test]
    async fn list_remove_at_effect_removes_index() {
        let actor = SpecDrivenActor::from_ioa(LIST_REMOVE_SPEC, HashMap::new()).expect("parse");
        let ctx = test_ctx("ListRemoveActor");
        let mut state = actor.initial_state();
        actor
            .handle(
                &ctx,
                &mut state,
                &test_message("AddTag", serde_json::json!({"tags": "a"})),
            )
            .await
            .expect("add a");
        actor
            .handle(
                &ctx,
                &mut state,
                &test_message("AddTag", serde_json::json!({"tags": "b"})),
            )
            .await
            .expect("add b");
        actor
            .handle(
                &ctx,
                &mut state,
                &test_message("DropTag", serde_json::json!({"tags_index": 0})),
            )
            .await
            .expect("drop");
        let s: SpecActorState = serde_json::from_slice(&state).expect("deser");
        assert_eq!(s.lists.get("tags").cloned().unwrap_or_default(), vec!["b"]);
    }
}
