//! Wrap [`temper_jit::apply::apply_effects`] and resolve leftover work.

use temper_jit::table::Effect;
use temper_runtime::scheduler::sim_now;

use super::{ScheduleAtRequest, ScheduledAction, SpawnRequest};
use crate::entity_actor::types::EntityState;

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
/// A tuple of (custom effect names, scheduled actions, spawn requests, schedule-at requests).
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
    let applied = temper_jit::apply::apply_effects(state, effects, params);

    for evt in &applied.emit {
        tracing::info!(
            entity_type = %state.entity_type,
            entity_id = %state.entity_id,
            event = %evt,
            "event emitted"
        );
    }
    for effect_name in &applied.custom {
        tracing::info!(
            entity_type = %state.entity_type,
            entity_id = %state.entity_id,
            effect = %effect_name,
            "custom effect (dispatched by post-transition hook)"
        );
    }
    for scheduled in &applied.scheduled {
        tracing::info!(
            entity_type = %state.entity_type,
            entity_id = %state.entity_id,
            scheduled_action = %scheduled.action,
            delay_seconds = scheduled.delay_seconds,
            "scheduled action (timer request)"
        );
    }
    for spawn in &applied.spawns {
        tracing::info!(
            entity_type = %state.entity_type,
            entity_id = %state.entity_id,
            child_type = %spawn.entity_type,
            child_id = %spawn.entity_id,
            "spawn entity request"
        );
    }
    for request in &applied.schedule_at {
        tracing::info!(
            entity_type = %state.entity_type,
            entity_id = %state.entity_id,
            scheduled_action = %request.action,
            field = %request.field,
            "schedule_at request (deferred until field resolution)"
        );
    }

    (
        applied.custom,
        applied.scheduled,
        applied.spawns,
        applied.schedule_at,
    )
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
