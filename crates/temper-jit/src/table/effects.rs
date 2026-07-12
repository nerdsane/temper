//! Canonical execution of the runtime effect vocabulary.
//!
//! The executor owns the single exhaustive match over [`Effect`]. Runtime backends
//! provide mutable state storage through [`EffectState`] and execute the returned
//! commands without reinterpreting effect semantics.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use temper_runtime::scheduler::sim_uuid;

use super::Effect;

/// Mutable storage required by the canonical effect executor.
pub trait EffectState {
    /// Current state-machine status.
    fn status(&self) -> &str;

    /// Mutable state-machine status.
    fn status_mut(&mut self) -> &mut String;

    /// Legacy item counter, when the backend retains one separately.
    fn legacy_item_count(&self) -> Option<usize>;

    /// Mutable legacy item counter, when the backend retains one separately.
    fn legacy_item_count_mut(&mut self) -> Option<&mut usize>;

    /// Named counters.
    fn counters(&self) -> &BTreeMap<String, usize>;

    /// Mutable named counters.
    fn counters_mut(&mut self) -> &mut BTreeMap<String, usize>;

    /// Named booleans.
    fn booleans(&self) -> &BTreeMap<String, bool>;

    /// Mutable named booleans.
    fn booleans_mut(&mut self) -> &mut BTreeMap<String, bool>;

    /// Named lists.
    fn lists(&self) -> &BTreeMap<String, Vec<String>>;

    /// Mutable named lists.
    fn lists_mut(&mut self) -> &mut BTreeMap<String, Vec<String>>;

    /// Arbitrary entity fields.
    fn fields(&self) -> &serde_json::Value;

    /// Mutable arbitrary entity fields.
    fn fields_mut(&mut self) -> &mut serde_json::Value;
}

/// Build the canonical guard-evaluation context from runtime state.
pub fn build_eval_context<S: EffectState>(state: &S) -> super::EvalContext {
    let mut context = super::EvalContext::default();
    if let Some(item_count) = state.legacy_item_count() {
        context.counters.insert("items".to_string(), item_count);
    }
    for (name, value) in state.counters() {
        context.counters.insert(name.clone(), *value);
    }
    for (name, value) in state.booleans() {
        context.booleans.insert(name.clone(), *value);
    }
    for (name, value) in state.lists() {
        context.lists.insert(name.clone(), value.clone());
    }
    context
}

/// A scheduled action to fire after a delay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledAction {
    /// The action name to dispatch.
    pub action: String,
    /// Delay in seconds before dispatching the action.
    pub delay_seconds: u64,
}

/// A deferred schedule-at request resolved by the runtime after field projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleAtRequest {
    /// The action name to dispatch.
    pub action: String,
    /// The entity field containing the absolute timestamp.
    pub field: String,
}

/// A request to spawn a child entity after a transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// The child entity type.
    pub entity_type: String,
    /// The resolved child entity ID.
    pub entity_id: String,
    /// Optional action to dispatch after creation.
    pub initial_action: Option<String>,
    /// Optional parent field that stores the child ID.
    pub store_id_in: Option<String>,
    /// Optional parent fields copied into the initial action.
    pub copy_fields: Option<Vec<String>>,
    /// Values copied from the parent at execution time.
    #[serde(default)]
    pub copied_field_values: serde_json::Map<String, serde_json::Value>,
}

/// Typed runtime commands produced while applying effects.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EffectExecution {
    /// Emitted domain events.
    pub emitted_events: Vec<String>,
    /// Custom runtime triggers.
    pub custom_effects: Vec<String>,
    /// Relative delayed actions.
    pub scheduled_actions: Vec<ScheduledAction>,
    /// Absolute-time actions awaiting field resolution.
    pub schedule_at_requests: Vec<ScheduleAtRequest>,
    /// Child entity creation requests.
    pub spawn_requests: Vec<SpawnRequest>,
}

/// Apply every effect to state and return commands that require a runtime driver.
///
/// This match is deliberately exhaustive. Adding an [`Effect`] variant must fail to
/// compile until its state and command semantics are defined here.
pub fn apply_effects<S: EffectState>(
    state: &mut S,
    effects: &[Effect],
    params: &serde_json::Value,
) -> EffectExecution {
    let mut execution = EffectExecution::default();

    for effect in effects {
        match effect {
            Effect::SetState(status) => *state.status_mut() = status.clone(),
            Effect::IncrementItems => increment_counter(state, "items", 1),
            Effect::DecrementItems => decrement_counter(state, "items", 1),
            Effect::IncrementCounter(var) => increment_counter(state, var, 1),
            Effect::IncrementCounterByParam { var, param } => {
                increment_counter(state, var, counter_delta_from_params(params, param));
            }
            Effect::DecrementCounter(var) => decrement_counter(state, var, 1),
            Effect::DecrementCounterByParam { var, param } => {
                decrement_counter(state, var, counter_delta_from_params(params, param));
            }
            Effect::SetCounterFromParam { var, param } => {
                if let Some(value) = counter_value_from_params(params, param) {
                    state.counters_mut().insert(var.clone(), value);
                    if var == "items"
                        && let Some(item_count) = state.legacy_item_count_mut()
                    {
                        *item_count = value;
                    }
                }
            }
            Effect::SetBool { var, value } => {
                state.booleans_mut().insert(var.clone(), *value);
            }
            Effect::EmitEvent(event) => execution.emitted_events.push(event.clone()),
            Effect::ListAppend(var) => {
                if let Some(value) = params.get(var).and_then(serde_json::Value::as_str) {
                    state
                        .lists_mut()
                        .entry(var.clone())
                        .or_default()
                        .push(value.to_string());
                }
            }
            Effect::ListRemoveAt(var) => {
                let index_param = format!("{var}_index");
                if let Some(index) = params
                    .get(index_param)
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| usize::try_from(value).ok())
                {
                    let list = state.lists_mut().entry(var.clone()).or_default();
                    if index < list.len() {
                        list.remove(index);
                    }
                }
            }
            Effect::Custom(effect_name) => {
                execution.custom_effects.push(effect_name.clone());
            }
            Effect::ScheduleAction {
                action,
                delay_seconds,
            } => execution.scheduled_actions.push(ScheduledAction {
                action: action.clone(),
                delay_seconds: *delay_seconds,
            }),
            Effect::ScheduleAtAction { action, field } => {
                execution.schedule_at_requests.push(ScheduleAtRequest {
                    action: action.clone(),
                    field: field.clone(),
                });
            }
            Effect::SpawnEntity {
                entity_type,
                entity_id_source,
                initial_action,
                store_id_in,
                copy_fields,
            } => {
                let child_id = resolve_child_id(params, entity_id_source);
                if let Some(field_name) = store_id_in
                    && let Some(fields) = state.fields_mut().as_object_mut()
                {
                    fields.insert(
                        field_name.clone(),
                        serde_json::Value::String(child_id.clone()),
                    );
                }

                let mut copied_field_values = serde_json::Map::new();
                if let Some(fields_to_copy) = copy_fields
                    && let Some(parent_fields) = state.fields().as_object()
                {
                    for field_name in fields_to_copy {
                        if let Some(value) = parent_fields.get(field_name) {
                            copied_field_values.insert(field_name.clone(), value.clone());
                        }
                    }
                }

                execution.spawn_requests.push(SpawnRequest {
                    entity_type: entity_type.clone(),
                    entity_id: child_id,
                    initial_action: initial_action.clone(),
                    store_id_in: store_id_in.clone(),
                    copy_fields: copy_fields.clone(),
                    copied_field_values,
                });
            }
        }
    }

    execution
}

fn increment_counter<S: EffectState>(state: &mut S, var: &str, delta: usize) {
    let counter = state.counters_mut().entry(var.to_string()).or_default();
    *counter = counter.saturating_add(delta);
    if var == "items"
        && let Some(item_count) = state.legacy_item_count_mut()
    {
        *item_count = item_count.saturating_add(delta);
    }
}

fn decrement_counter<S: EffectState>(state: &mut S, var: &str, delta: usize) {
    let counter = state.counters_mut().entry(var.to_string()).or_default();
    *counter = counter.saturating_sub(delta);
    if var == "items"
        && let Some(item_count) = state.legacy_item_count_mut()
    {
        *item_count = item_count.saturating_sub(delta);
    }
}

fn counter_delta_from_params(params: &serde_json::Value, param: &str) -> usize {
    params
        .get(param)
        .and_then(|value| match value {
            serde_json::Value::Number(number) => number
                .as_u64()
                .and_then(|value| usize::try_from(value).ok()),
            serde_json::Value::String(text) => text.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(0)
}

fn counter_value_from_params(params: &serde_json::Value, param: &str) -> Option<usize> {
    params
        .get(param)
        .and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
        })
        .and_then(|value| usize::try_from(value).ok())
}

fn resolve_child_id(params: &serde_json::Value, source: &str) -> String {
    if source == "{uuid}" {
        return sim_uuid().to_string();
    }
    params
        .get(source)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| sim_uuid().to_string())
}

#[cfg(test)]
#[path = "effects/tests.rs"]
mod tests;
