//! Shared effect application — the single source of truth.
//!
//! This module contains the ONE function that mutates [`EntityState`] in response
//! to transition effects. It is called by:
//! - Production actor handle (`EntityActor::handle`)
//! - Production event replay (`EntityActor::replay_events`)
//! - Deterministic simulation (`EntityActorHandler::handle_message`)
//!
//! **FoundationDB DST principle**: The exact same code path must run in both
//! production and simulation. Having a single `apply_effects()` function
//! guarantees that simulation tests exercise the real production logic.

use temper_jit::table::Effect;

use super::types::EntityState;

/// Apply a list of transition effects to entity state.
///
/// This is the canonical effect-application logic. All code paths that mutate
/// entity state after a successful `TransitionTable::evaluate()` MUST call
/// this function. Do not duplicate this logic elsewhere.
///
/// # Arguments
/// - `state` — The entity state to mutate.
/// - `effects` — The effects returned by `TransitionTable::evaluate()`.
/// - `params` — The action parameters (needed for `ListAppend` / `ListRemoveAt`).
///
/// # Returns
/// A list of custom effect names (for post-transition hook dispatch).
pub fn apply_effects(
    state: &mut EntityState,
    effects: &[Effect],
    params: &serde_json::Value,
) -> Vec<String> {
    let mut custom_effects = Vec::new();

    for effect in effects {
        match effect {
            Effect::SetState(s) => {
                state.status = s.clone();
            }
            Effect::IncrementItems => {
                state.item_count += 1;
                *state.counters.entry("items".to_string()).or_default() += 1;
            }
            Effect::DecrementItems => {
                state.item_count = state.item_count.saturating_sub(1);
                let c = state.counters.entry("items".to_string()).or_default();
                *c = c.saturating_sub(1);
            }
            Effect::IncrementCounter(var) => {
                *state.counters.entry(var.clone()).or_default() += 1;
                // Keep legacy item_count in sync.
                if var == "items" {
                    state.item_count += 1;
                }
            }
            Effect::DecrementCounter(var) => {
                let c = state.counters.entry(var.clone()).or_default();
                *c = c.saturating_sub(1);
                if var == "items" {
                    state.item_count = state.item_count.saturating_sub(1);
                }
            }
            Effect::SetBool { var, value } => {
                state.booleans.insert(var.clone(), *value);
            }
            Effect::ListAppend(var) => {
                if let Some(val) = params.get(var).and_then(|v| v.as_str()) {
                    state
                        .lists
                        .entry(var.clone())
                        .or_default()
                        .push(val.to_string());
                }
            }
            Effect::ListRemoveAt(var) => {
                let index_key = format!("{var}_index");
                if let Some(idx) = params.get(&index_key).and_then(|v| v.as_u64()) {
                    let list = state.lists.entry(var.clone()).or_default();
                    let idx = idx as usize;
                    if idx < list.len() {
                        list.remove(idx);
                    }
                }
            }
            Effect::EmitEvent(evt) => {
                tracing::info!(
                    entity_type = %state.entity_type,
                    entity_id = %state.entity_id,
                    event = %evt,
                    "event emitted"
                );
            }
            Effect::Custom(effect_name) => {
                custom_effects.push(effect_name.clone());
                tracing::info!(
                    entity_type = %state.entity_type,
                    entity_id = %state.entity_id,
                    effect = %effect_name,
                    "custom effect (dispatched by post-transition hook)"
                );
            }
        }
    }

    custom_effects
}

/// Apply the `new_state` fallback from a TransitionResult.
///
/// If no `Effect::SetState` was applied (status unchanged from `from_status`)
/// and the transition result provides a `new_state`, apply it.
pub fn apply_new_state_fallback(state: &mut EntityState, from_status: &str, new_state: &str) {
    if state.status == from_status && !new_state.is_empty() {
        state.status = new_state.to_string();
    }
}

/// Sync all state variables into the `fields` JSON object.
///
/// This projects status, counters, booleans, lists, and action params
/// into the entity's fields for OData queries.
pub fn sync_fields(state: &mut EntityState, params: &serde_json::Value) {
    if let Some(obj) = state.fields.as_object_mut() {
        obj.insert(
            "Status".to_string(),
            serde_json::Value::String(state.status.clone()),
        );
        // Project action params into fields
        if let Some(p) = params.as_object() {
            for (k, v) in p {
                obj.insert(k.clone(), v.clone());
            }
        }
        // Sync counters into fields
        for (k, v) in &state.counters {
            obj.insert(
                k.clone(),
                serde_json::Value::Number((*v as u64).into()),
            );
        }
        // Sync booleans into fields
        for (k, v) in &state.booleans {
            obj.insert(k.clone(), serde_json::Value::Bool(*v));
        }
        // Sync lists into fields
        for (k, v) in &state.lists {
            let arr: Vec<serde_json::Value> = v
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect();
            obj.insert(k.clone(), serde_json::Value::Array(arr));
        }
    }
}
