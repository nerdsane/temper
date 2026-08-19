//! Evaluate a transition and produce a [`ProcessResult`].

use sha2::{Digest, Sha256};
use temper_jit::table::{EvalContext, GuardFailure, TransitionTable};
use temper_runtime::scheduler::sim_now;

use super::apply::{apply_effects, apply_new_state_fallback, resolve_schedule_at_requests};
use super::fields::{FieldSyncMode, sync_fields_with_metadata};
use super::{ScheduledAction, SpawnRequest};
use crate::blobs::OverflowBlobWrite;
use crate::entity_actor::types::{EntityEvent, EntityState, MAX_EVENTS_SINCE_SNAPSHOT};

/// Maximum cross-entity lookups per transition (TigerStyle budget).
///
/// Rich contracts may legitimately verify several sibling entities in one
/// guarded transition.
pub const MAX_CROSS_ENTITY_LOOKUPS: usize = 16;
/// Maximum entity spawns per transition (TigerStyle budget).
pub const MAX_SPAWNS_PER_TRANSITION: usize = 8;

/// Build an [`EvalContext`] from current entity state.
///
/// This is the single source of truth for context construction. All code paths
/// that call `TransitionTable::evaluate_ctx()` MUST use this function.
pub fn build_eval_context(state: &EntityState) -> EvalContext {
    build_eval_context_with_xref(state, &std::collections::BTreeMap::new())
}

/// Build an [`EvalContext`] with pre-resolved cross-entity booleans.
///
/// The `cross_entity_booleans` map contains `__xref:{type}:{field} -> bool` entries
/// from cross-entity state gate resolution at the dispatch layer.
pub fn build_eval_context_with_xref(
    state: &EntityState,
    cross_entity_booleans: &std::collections::BTreeMap<String, bool>,
) -> EvalContext {
    let mut ctx = EvalContext::default();
    ctx.counters.insert("items".to_string(), state.item_count);
    for (k, v) in &state.counters {
        ctx.counters.insert(k.clone(), *v);
    }
    for (k, v) in &state.booleans {
        ctx.booleans.insert(k.clone(), *v);
    }
    for (k, v) in &state.lists {
        ctx.lists.insert(k.clone(), v.clone());
    }
    // Merge pre-resolved cross-entity state booleans
    for (k, v) in cross_entity_booleans {
        ctx.booleans.insert(k.clone(), *v);
    }
    ctx
}

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
            let sanitized_params = sanitize_action_params(effective_params.as_ref());
            let params = sanitized_params.as_ref();

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

/// Remove caller fields whose values are derived authoritatively by the runtime.
///
/// The returned value borrows the input when no reserved keys are present and
/// clones only when sanitization is required. Persisted events therefore never
/// acquire a second mutable identity, lifecycle, or context-status truth.
pub(crate) fn sanitize_action_params(
    params: &serde_json::Value,
) -> std::borrow::Cow<'_, serde_json::Value> {
    let Some(fields) = params.as_object() else {
        return std::borrow::Cow::Borrowed(params);
    };
    if !fields
        .keys()
        .any(|key| temper_spec::automaton::is_server_derived_field_name(key))
    {
        return std::borrow::Cow::Borrowed(params);
    }
    let mut sanitized = fields.clone();
    sanitized.retain(|key, _| !temper_spec::automaton::is_server_derived_field_name(key));
    std::borrow::Cow::Owned(serde_json::Value::Object(sanitized))
}

/// Compute the exact actor-state precondition bound to an external field-update
/// authorization decision.
///
/// The digest includes the durable sequence and the authorization-visible local
/// lifecycle/field state. Recursive canonical JSON hashing keeps the result
/// deterministic even when callers construct object keys in different orders.
pub(crate) fn entity_authorization_precondition(state: &EntityState) -> String {
    fn update_bytes(hasher: &mut Sha256, bytes: &[u8]) {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        hasher.update(len.to_be_bytes());
        hasher.update(bytes);
    }

    fn update_json(hasher: &mut Sha256, value: &serde_json::Value) {
        match value {
            serde_json::Value::Null => hasher.update([0]),
            serde_json::Value::Bool(value) => hasher.update([1, u8::from(*value)]),
            serde_json::Value::Number(value) => {
                hasher.update([2]);
                update_bytes(hasher, value.to_string().as_bytes());
            }
            serde_json::Value::String(value) => {
                hasher.update([3]);
                update_bytes(hasher, value.as_bytes());
            }
            serde_json::Value::Array(values) => {
                hasher.update([4]);
                hasher.update(
                    u64::try_from(values.len())
                        .unwrap_or(u64::MAX)
                        .to_be_bytes(),
                );
                for value in values {
                    update_json(hasher, value);
                }
            }
            serde_json::Value::Object(values) => {
                hasher.update([5]);
                hasher.update(
                    u64::try_from(values.len())
                        .unwrap_or(u64::MAX)
                        .to_be_bytes(),
                );
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                for key in keys {
                    update_bytes(hasher, key.as_bytes());
                    if let Some(value) = values.get(key) {
                        update_json(hasher, value);
                    }
                }
            }
        }
    }

    let mut hasher = Sha256::new();
    hasher.update(b"temper-entity-authorization-precondition-v1");
    hasher.update(state.sequence_nr.to_be_bytes());
    update_bytes(&mut hasher, state.status.as_bytes());
    update_json(&mut hasher, &state.fields);
    format!("{:x}", hasher.finalize())
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
