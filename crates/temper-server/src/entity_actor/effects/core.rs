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

use temper_jit::table::{Effect, GuardFailure, TransitionTable};
use temper_runtime::scheduler::sim_now;

pub use temper_jit::table::{ScheduleAtRequest, ScheduledAction, SpawnRequest};

use crate::blobs::OverflowBlobWrite;

use super::super::types::{EntityEvent, EntityState, MAX_EVENTS_SINCE_SNAPSHOT};
use super::canonical::build_eval_context_with_xref;
use super::fields::{DEFAULT_FIELD_INLINE_MAX, sync_fields_with_metadata};

/// Result of processing an action through the transition table.
#[derive(Debug, Clone)]
pub struct ProcessResult {
    /// Whether the action succeeded.
    pub success: bool,
    /// The event recording the transition (if successful).
    pub event: Option<EntityEvent>,
    /// Custom effects for post-transition hook dispatch.
    pub custom_effects: Vec<String>,
    /// Scheduled actions to fire after delays (for timer dispatch).
    pub scheduled_actions: Vec<ScheduledAction>,
    /// Spawn requests for child entities.
    pub spawn_requests: Vec<SpawnRequest>,
    /// Deferred blob writes for oversized projected field values.
    pub overflow_blobs: Vec<OverflowBlobWrite>,
    /// Error message (if action failed).
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSyncMode {
    /// Values exceeding the inline ceiling are replaced with a placeholder string.
    /// Used by stores without blob-backed overflow (Postgres, memory).
    InlineTruncate,
    /// Values exceeding `default_inline_max` are written to the Turso blob store
    /// and replaced with a content-addressed reference object.
    BlobRefs { default_inline_max: usize },
}

impl FieldSyncMode {
    /// Construct `BlobRefs` with the crate-wide default ceiling.
    pub fn blob_refs_default() -> Self {
        FieldSyncMode::BlobRefs {
            default_inline_max: DEFAULT_FIELD_INLINE_MAX,
        }
    }

    /// Returns the inline-size ceiling in bytes for this mode.
    pub(super) fn inline_max(self) -> usize {
        match self {
            FieldSyncMode::InlineTruncate => DEFAULT_FIELD_INLINE_MAX,
            FieldSyncMode::BlobRefs { default_inline_max } => default_inline_max,
        }
    }
}

/// Process an action through the transition table.
///
/// This is the core business logic — evaluate guard, apply effects, construct event.
/// Production adds persistence + telemetry around this.
/// Simulation calls it directly.
/// Replay uses `build_eval_context` but handles stored events specially.
///
/// **FoundationDB DST principle**: one function for all code paths.
pub fn process_action(
    state: &mut EntityState,
    table: &TransitionTable,
    action: &str,
    params: &serde_json::Value,
) -> ProcessResult {
    process_action_with_xref_and_field_mode(
        state,
        table,
        action,
        params,
        &std::collections::BTreeMap::new(),
        FieldSyncMode::InlineTruncate,
    )
}

/// Render a [`GuardFailure`] into the agent-facing self-heal error string (ADR-0151).
///
/// Names the action, the from-state, the specific sub-guard that failed, the
/// field/ref it read, and the required-vs-found values where the guard exposes
/// them, e.g.:
///
/// `Action 'SubmitForReview' blocked from state 'Draft': guard cross_entity_state on 'landing_file_id' requires File status in [Ready,Locked], found <unsatisfied>`
fn render_guard_failure(action: &str, from_state: &str, failure: &GuardFailure) -> String {
    let mut msg = format!(
        "Action '{action}' blocked from state '{from_state}': guard {}",
        failure.kind.label()
    );
    if let Some(var) = &failure.var {
        msg.push_str(&format!(" on '{var}'"));
    }
    if let Some(required) = &failure.required {
        msg.push_str(&format!(" requires {required}"));
    }
    if let Some(found) = &failure.found {
        msg.push_str(&format!(", found {found}"));
    }
    msg
}

/// Process an action with pre-resolved cross-entity booleans.
///
/// Same as [`process_action`] but injects cross-entity state booleans
/// into the evaluation context for `CrossEntityStateIn` guard evaluation.
pub fn process_action_with_xref(
    state: &mut EntityState,
    table: &TransitionTable,
    action: &str,
    params: &serde_json::Value,
    cross_entity_booleans: &std::collections::BTreeMap<String, bool>,
) -> ProcessResult {
    process_action_with_xref_and_field_mode(
        state,
        table,
        action,
        params,
        cross_entity_booleans,
        FieldSyncMode::InlineTruncate,
    )
}

pub fn process_action_with_xref_and_field_mode(
    state: &mut EntityState,
    table: &TransitionTable,
    action: &str,
    params: &serde_json::Value,
    cross_entity_booleans: &std::collections::BTreeMap<String, bool>,
    field_sync_mode: FieldSyncMode,
) -> ProcessResult {
    if state.events_since_snapshot >= MAX_EVENTS_SINCE_SNAPSHOT {
        return ProcessResult {
            success: false,
            event: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            overflow_blobs: vec![],
            error: Some(format!(
                "Event budget exhausted ({MAX_EVENTS_SINCE_SNAPSHOT} max since snapshot)"
            )),
        };
    }

    let ctx = build_eval_context_with_xref(state, cross_entity_booleans);
    let result = table.evaluate_ctx(&state.status, &ctx, action);

    match result {
        Some(transition_result) if transition_result.success => {
            let from_status = state.status.clone();
            let to_status = transition_result.new_state.clone();

            if let Some(error) = validate_ref_action_contract(state, action, params) {
                return ProcessResult {
                    success: false,
                    event: None,
                    custom_effects: vec![],
                    scheduled_actions: vec![],
                    spawn_requests: vec![],
                    overflow_blobs: vec![],
                    error: Some(error),
                };
            }

            let effective_params = normalize_ref_action_params(state, action, params);
            let params = effective_params.as_ref();

            let (custom_effects, scheduled_actions, spawn_requests, schedule_at_requests) =
                apply_effects(state, &transition_result.effects, params);
            apply_new_state_fallback(state, &from_status, &to_status);
            let overflow_blobs = sync_fields_with_metadata(
                state,
                params,
                field_sync_mode,
                Some(&table.state_var_metadata),
            );

            // Resolve deferred schedule_at requests now that fields are synced
            let mut all_scheduled = scheduled_actions;
            all_scheduled.extend(resolve_schedule_at_requests(state, &schedule_at_requests));

            let event = EntityEvent {
                action: action.to_string(),
                from_status,
                to_status: state.status.clone(),
                timestamp: sim_now(),
                params: params.clone(),
                idempotency_key: None,
            };

            ProcessResult {
                success: true,
                event: Some(event),
                custom_effects,
                scheduled_actions: all_scheduled,
                spawn_requests,
                overflow_blobs,
                error: None,
            }
        }
        Some(rejected) => ProcessResult {
            success: false,
            event: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            overflow_blobs: vec![],
            // ADR-0151: when a sub-guard failed (rule matched by name and
            // state, but a precondition did not hold) name the specific guard,
            // field/ref, and required-vs-found so an in-session agent can
            // self-heal. A from-state miss carries no guard failure and keeps
            // the generic message.
            error: Some(match &rejected.guard_failure {
                Some(failure) => render_guard_failure(action, &state.status, failure),
                None => format!(
                    "Action '{}' not valid from state '{}'",
                    action, state.status
                ),
            }),
        },
        None => ProcessResult {
            success: false,
            event: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            overflow_blobs: vec![],
            error: Some(format!("Unknown action: {}", action)),
        },
    }
}

fn validate_ref_action_contract(
    state: &EntityState,
    action: &str,
    params: &serde_json::Value,
) -> Option<String> {
    if state.entity_type != "Ref" {
        return None;
    }

    match action {
        "Update" => {
            let Some(expected) =
                json_string_param(params, "PreviousCommitSha").filter(|value| !value.is_empty())
            else {
                return Some("Ref.Update requires PreviousCommitSha".to_string());
            };
            if json_string_param(params, "NewCommitSha")
                .filter(|value| !value.is_empty())
                .is_none()
            {
                return Some("Ref.Update requires NewCommitSha".to_string());
            }

            let current = current_ref_target(state);
            if ref_previous_matches_current(current, &expected) {
                None
            } else {
                Some(format!(
                    "stale ref {}: expected {}, found {}",
                    state.entity_id,
                    expected,
                    current.unwrap_or("missing ref")
                ))
            }
        }
        "ForceUpdate" => {
            if json_string_param(params, "NewCommitSha")
                .filter(|value| !value.is_empty())
                .is_none()
            {
                Some("Ref.ForceUpdate requires NewCommitSha".to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn normalize_ref_action_params<'a>(
    state: &EntityState,
    action: &str,
    params: &'a serde_json::Value,
) -> std::borrow::Cow<'a, serde_json::Value> {
    if state.entity_type != "Ref" || !matches!(action, "Update" | "ForceUpdate") {
        return std::borrow::Cow::Borrowed(params);
    }

    let Some(new_commit_sha) = json_string_param(params, "NewCommitSha") else {
        return std::borrow::Cow::Borrowed(params);
    };

    let mut normalized = params.clone();
    if let Some(obj) = normalized.as_object_mut() {
        obj.insert(
            "TargetCommitSha".to_string(),
            serde_json::Value::String(new_commit_sha),
        );
        std::borrow::Cow::Owned(normalized)
    } else {
        std::borrow::Cow::Borrowed(params)
    }
}

fn current_ref_target(state: &EntityState) -> Option<&str> {
    state
        .fields
        .get("TargetCommitSha")
        .and_then(serde_json::Value::as_str)
        .filter(|sha| !sha.is_empty())
}

fn ref_previous_matches_current(current: Option<&str>, expected: &str) -> bool {
    if is_zero_git_sha(expected) {
        return current.is_none() || current.is_some_and(is_zero_git_sha);
    }
    current == Some(expected)
}

fn is_zero_git_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte == b'0')
}

fn json_string_param(params: &serde_json::Value, field: &str) -> Option<String> {
    params.get(field).and_then(|value| match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

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
    let execution = temper_jit::table::apply_effects(state, effects, params);

    for event in &execution.emitted_events {
        tracing::info!(
            entity_type = %state.entity_type,
            entity_id = %state.entity_id,
            event,
            "event emitted"
        );
    }
    for effect in &execution.custom_effects {
        tracing::info!(
            entity_type = %state.entity_type,
            entity_id = %state.entity_id,
            effect,
            "custom effect (dispatched by post-transition hook)"
        );
    }
    for scheduled in &execution.scheduled_actions {
        tracing::info!(
            entity_type = %state.entity_type,
            entity_id = %state.entity_id,
            scheduled_action = %scheduled.action,
            delay_seconds = scheduled.delay_seconds,
            "scheduled action (timer request)"
        );
    }
    for spawn in &execution.spawn_requests {
        tracing::info!(
            entity_type = %state.entity_type,
            entity_id = %state.entity_id,
            child_type = %spawn.entity_type,
            child_id = %spawn.entity_id,
            "spawn entity request"
        );
    }
    for schedule_at in &execution.schedule_at_requests {
        tracing::info!(
            entity_type = %state.entity_type,
            entity_id = %state.entity_id,
            scheduled_action = %schedule_at.action,
            field = %schedule_at.field,
            "schedule_at request (deferred until field resolution)"
        );
    }

    (
        execution.custom_effects,
        execution.scheduled_actions,
        execution.spawn_requests,
        execution.schedule_at_requests,
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
