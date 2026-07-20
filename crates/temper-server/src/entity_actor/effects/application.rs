//! Transition effect application.

use super::*;

pub fn apply_effects(
    state: &mut EntityState,
    effects: &[Effect],
    params: &serde_json::Value,
) -> (
    Vec<String>,
    Vec<ScheduledAction>,
    Vec<SpawnRequest>,
    Vec<ScheduleAtRequest>,
) {
    let mut custom_effects = Vec::new();
    let mut scheduled_actions = Vec::new();
    let mut spawn_requests = Vec::new();
    let mut schedule_at_requests = Vec::new();

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
            Effect::IncrementCounterByParam { var, param } => {
                let delta = counter_delta_from_params(params, param);
                *state.counters.entry(var.clone()).or_default() += delta;
                if var == "items" {
                    state.item_count += delta;
                }
            }
            Effect::DecrementCounter(var) => {
                let c = state.counters.entry(var.clone()).or_default();
                *c = c.saturating_sub(1);
                if var == "items" {
                    state.item_count = state.item_count.saturating_sub(1);
                }
            }
            Effect::DecrementCounterByParam { var, param } => {
                let delta = counter_delta_from_params(params, param);
                let c = state.counters.entry(var.clone()).or_default();
                *c = c.saturating_sub(delta);
                if var == "items" {
                    state.item_count = state.item_count.saturating_sub(delta);
                }
            }
            Effect::SetCounterFromParam { var, param } => {
                let parsed = params
                    .get(param)
                    .and_then(|v| {
                        v.as_u64()
                            .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
                    })
                    .and_then(|n| usize::try_from(n).ok());
                match parsed {
                    Some(value) => {
                        state.counters.insert(var.clone(), value);
                        if var == "items" {
                            state.item_count = value;
                        }
                    }
                    None => tracing::warn!(
                        entity_type = %state.entity_type,
                        entity_id = %state.entity_id,
                        counter = %var,
                        param = %param,
                        "set_counter_from_param skipped because param was missing or not a non-negative integer"
                    ),
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
            Effect::ScheduleAction {
                action,
                delay_seconds,
            } => {
                scheduled_actions.push(ScheduledAction {
                    action: action.clone(),
                    delay_seconds: *delay_seconds,
                });
                tracing::info!(
                    entity_type = %state.entity_type,
                    entity_id = %state.entity_id,
                    scheduled_action = %action,
                    delay_seconds,
                    "scheduled action (timer request)"
                );
            }
            Effect::SpawnEntity {
                entity_type,
                entity_id_source,
                initial_action,
                store_id_in,
                copy_fields,
            } => {
                // Resolve child entity ID from params or generate UUID
                let child_id = if entity_id_source == "{uuid}" {
                    sim_uuid().to_string()
                } else {
                    params
                        .get(entity_id_source)
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| sim_uuid().to_string())
                };

                // Store child ID in parent's fields if requested
                if let Some(field_name) = store_id_in
                    && let Some(obj) = state.fields.as_object_mut()
                {
                    obj.insert(
                        field_name.clone(),
                        serde_json::Value::String(child_id.clone()),
                    );
                }

                // Copy named fields from parent state into spawn request
                let mut copied_field_values = serde_json::Map::new();
                if let Some(fields_to_copy) = copy_fields
                    && let Some(parent_obj) = state.fields.as_object()
                {
                    for field_name in fields_to_copy {
                        if let Some(value) = parent_obj.get(field_name) {
                            copied_field_values.insert(field_name.clone(), value.clone());
                        }
                    }
                }

                spawn_requests.push(SpawnRequest {
                    entity_type: entity_type.clone(),
                    entity_id: child_id.clone(),
                    initial_action: initial_action.clone(),
                    store_id_in: store_id_in.clone(),
                    copy_fields: copy_fields.clone(),
                    copied_field_values,
                });

                tracing::info!(
                    entity_type = %state.entity_type,
                    entity_id = %state.entity_id,
                    child_type = %entity_type,
                    child_id = %child_id,
                    "spawn entity request"
                );
            }
            Effect::ScheduleAtAction { action, field } => {
                schedule_at_requests.push(ScheduleAtRequest {
                    action: action.clone(),
                    field: field.clone(),
                });
                tracing::info!(
                    entity_type = %state.entity_type,
                    entity_id = %state.entity_id,
                    scheduled_action = %action,
                    field = %field,
                    "schedule_at request (deferred until field resolution)"
                );
            }
        }
    }

    (
        custom_effects,
        scheduled_actions,
        spawn_requests,
        schedule_at_requests,
    )
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

/// Resolve deferred `schedule_at` requests into [`ScheduledAction`]s.
///
/// Must be called AFTER [`sync_fields`] so that entity fields contain
/// the latest param values. Reads the named field as an ISO 8601
/// timestamp, computes `delay = target - now` (clamped to 0 if past).
pub fn resolve_schedule_at_requests(
    state: &EntityState,
    requests: &[ScheduleAtRequest],
) -> Vec<ScheduledAction> {
    if requests.is_empty() {
        return Vec::new();
    }
    let now = sim_now();
    requests
        .iter()
        .filter_map(|req| {
            let field_value = state.fields.get(&req.field).and_then(|v| v.as_str())?;
            let target = chrono::DateTime::parse_from_rfc3339(field_value)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .or_else(|| {
                    // Fallback: try parsing without timezone suffix (assume UTC)
                    chrono::NaiveDateTime::parse_from_str(field_value, "%Y-%m-%dT%H:%M:%S")
                        .ok()
                        .map(|ndt| ndt.and_utc())
                })?;
            let delay_seconds = (target - now).num_seconds().max(0) as u64;
            tracing::info!(
                entity_type = %state.entity_type,
                entity_id = %state.entity_id,
                action = %req.action,
                field = %req.field,
                target = %target,
                delay_seconds,
                "schedule_at resolved"
            );
            Some(ScheduledAction {
                action: req.action.clone(),
                delay_seconds,
            })
        })
        .collect()
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
