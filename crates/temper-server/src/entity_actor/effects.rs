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

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use temper_jit::table::{Effect, EvalContext, GuardFailure, TransitionTable};
use temper_runtime::scheduler::{sim_now, sim_uuid};

use crate::blobs::{FIELD_OVERFLOW_BLOB_PREFIX, OverflowBlobWrite, blob_ref_value};

use super::types::{EntityEvent, EntityState, MAX_EVENTS_SINCE_SNAPSHOT};

/// A scheduled action to fire after a delay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledAction {
    /// The action name to dispatch.
    pub action: String,
    /// Delay in seconds before dispatching the action.
    pub delay_seconds: u64,
}

/// A request to spawn a child entity (executed post-transition by dispatch pipeline).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// The child entity type.
    pub entity_type: String,
    /// The child entity ID.
    pub entity_id: String,
    /// Optional action to dispatch on the child after creation.
    pub initial_action: Option<String>,
    /// Optional field on the parent to store the child's ID.
    pub store_id_in: Option<String>,
    /// Optional list of field names to copy from parent state into child's initial_action params.
    pub copy_fields: Option<Vec<String>>,
    /// Field values copied from parent state (populated when copy_fields is Some).
    #[serde(default)]
    pub copied_field_values: serde_json::Map<String, serde_json::Value>,
}

/// A deferred schedule-at request — resolved after `sync_fields`.
///
/// Unlike `ScheduledAction` (fixed delay), a `ScheduleAtRequest` reads an absolute
/// ISO 8601 timestamp from an entity field and computes the delay at resolution time.
#[derive(Debug, Clone)]
pub struct ScheduleAtRequest {
    /// The action name to dispatch.
    pub action: String,
    /// The entity field containing the ISO 8601 timestamp.
    pub field: String,
}

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
    fn inline_max(self) -> usize {
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

    // ARN-247: restrict caller params to the action's declared set BEFORE guard
    // evaluation, effect application, field projection, and event recording — so
    // this single chokepoint (shared by every live dispatch path: OData bound
    // actions, composite sub-writes, spawn initial actions, DST sim) can only
    // write what the verification cascade modeled. Recording the filtered params
    // on the event also keeps replay faithful (replay re-projects `event.params`
    // directly). Runs ahead of `normalize_ref_action_params` so kernel-derived
    // params survive.
    let restricted = restrict_to_declared_params(table, action, &state.entity_type, params);
    let params = restricted.as_ref();

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

/// ARN-247: canonical form for matching an action's declared param name against
/// a request-body key.
///
/// Lowercase and drop underscores so the two Temper naming conventions for one
/// logical field compare equal: IOA `[[action]] params` are snake_case *logical*
/// names, while some callers send the PascalCase field names they actually store
/// — e.g. paw-fs dispatches `Directory.Create` with `WorkspaceId`/`ParentId` for
/// declared `workspace_id`/`parent_id` ("the snake_case `params` list names the
/// action's logical inputs but does not rename or gate them"). Matching up to
/// this convention keeps those callers working while a genuinely undeclared key
/// (e.g. `goal` smuggled into an action that never declared it) still matches no
/// declared param.
fn normalized_param_key(key: &str) -> String {
    key.chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// ARN-247: whether the runtime may keep request-body key `key` for `action`.
///
/// Allowed iff `key` matches a declared param up to naming convention
/// ([`normalized_param_key`]), or is a transient action field. **Transient action
/// fields** (large/ephemeral inputs consumed by triggers but never persisted,
/// e.g. `Repository` pack bytes) are kept so trigger dispatch still sees them,
/// mirroring [`is_transient_action_field`]. The drop filter
/// ([`restrict_to_declared_params`]) and the OData-boundary reject
/// ([`undeclared_param_keys`]) share this one rule so they never drift.
///
/// `normalized_declared` is the action's declared param set pre-normalized by
/// the caller so it is built once per dispatch, not once per key.
fn param_key_allowed(
    normalized_declared: &std::collections::BTreeSet<String>,
    entity_type: &str,
    key: &str,
) -> bool {
    normalized_declared.contains(&normalized_param_key(key))
        || is_transient_action_field(entity_type, key)
}

/// ARN-247: restrict caller-supplied action params to the action's declared set.
///
/// The verification cascade models an action as writing only its declared
/// params; without this, the runtime projected *every* request-body key into
/// entity fields ([`sync_fields`]), so a proven invariant was violable. This
/// drops any key that is not allowed by [`param_key_allowed`] — so the runtime
/// can only write what was verified.
///
/// **Kernel-injected params survive by construction, not by exemption here:**
/// the `Ref` `TargetCommitSha` is derived by [`normalize_ref_action_params`],
/// which runs *after* this filter; spawn's `parent_id`/`parent_type` and its
/// `copy_fields` values are persisted into the child at creation (via
/// `initial_fields`, which is not action dispatch and so never reaches this
/// filter — see `dispatch/cross_entity.rs`). This filter only ever restricts the
/// caller-supplied action body. When the table carries no declaration for
/// `action` ([`TransitionTable::declared_params`] returns `None` — an older
/// deserialized table, or a synthetic kernel action) params pass through
/// unchanged, preserving prior behavior.
fn restrict_to_declared_params<'a>(
    table: &TransitionTable,
    action: &str,
    entity_type: &str,
    params: &'a serde_json::Value,
) -> std::borrow::Cow<'a, serde_json::Value> {
    let Some(declared) = table.declared_params(action) else {
        return std::borrow::Cow::Borrowed(params);
    };
    let Some(obj) = params.as_object() else {
        return std::borrow::Cow::Borrowed(params);
    };
    let normalized_declared: std::collections::BTreeSet<String> =
        declared.iter().map(|d| normalized_param_key(d)).collect();
    // Fast path: nothing undeclared — avoid the clone.
    if obj
        .keys()
        .all(|k| param_key_allowed(&normalized_declared, entity_type, k))
    {
        return std::borrow::Cow::Borrowed(params);
    }
    let filtered: serde_json::Map<String, serde_json::Value> = obj
        .iter()
        .filter(|(k, _)| param_key_allowed(&normalized_declared, entity_type, k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    std::borrow::Cow::Owned(serde_json::Value::Object(filtered))
}

/// ARN-247: the request-body keys `action` does not declare and that are not
/// transient action fields — i.e. exactly the keys the runtime would silently
/// drop via [`restrict_to_declared_params`]. The OData boundary rejects a bound
/// action carrying any of these (400) so external caller bugs surface loudly,
/// while internal dispatch (spawn, composite) keeps dropping them. Empty when
/// the table declares no params for the action (unknown/synthetic action) or the
/// body is not a JSON object — in those cases the caller must not reject.
pub(crate) fn undeclared_param_keys(
    table: &TransitionTable,
    action: &str,
    entity_type: &str,
    body: &serde_json::Value,
) -> Vec<String> {
    let Some(declared) = table.declared_params(action) else {
        return Vec::new();
    };
    let Some(obj) = body.as_object() else {
        return Vec::new();
    };
    let normalized_declared: std::collections::BTreeSet<String> =
        declared.iter().map(|d| normalized_param_key(d)).collect();
    obj.keys()
        .filter(|k| !param_key_allowed(&normalized_declared, entity_type, k))
        .cloned()
        .collect()
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

/// Default inline ceiling for a single field value projected into entity state.
///
/// Values above this size are either truncated (`InlineTruncate`) or moved to
/// the content-addressed blob store (`BlobRefs`) per ADR-0040. The ceiling is
/// sized to fit comfortably inside `CTX_BUF_LEN` (512 KB) with headroom for the
/// rest of `entity_state` (counters, booleans, lists, other fields) while
/// covering p99 of observed oversize-field traffic.
///
/// See ADR-0045.
pub const DEFAULT_FIELD_INLINE_MAX: usize = 131_072; // 128 KB

/// Sync all state variables into the `fields` JSON object.
///
/// Projects status, counters, booleans, lists, and action params into the
/// entity's fields for OData queries. Values whose serialized size exceeds
/// the effective per-field inline ceiling are either truncated or projected
/// through blob refs, depending on `mode`. When `state_var_metadata` is
/// `Some`, per-field `overflow_inline_max_bytes` and `overflow_ttl_seconds`
/// overrides are consulted (ADR-0045, ADR-0047).
pub fn sync_fields(
    state: &mut EntityState,
    params: &serde_json::Value,
    mode: FieldSyncMode,
) -> Vec<OverflowBlobWrite> {
    sync_fields_with_metadata(state, params, mode, None)
}

/// Metadata-aware variant of [`sync_fields`]. Threads per-field overflow
/// declarations from the IOA spec's `[[state]]` blocks into the projection.
pub fn sync_fields_with_metadata(
    state: &mut EntityState,
    params: &serde_json::Value,
    mode: FieldSyncMode,
    state_var_metadata: Option<
        &std::collections::BTreeMap<String, temper_jit::table::StateVarMetadata>,
    >,
) -> Vec<OverflowBlobWrite> {
    let mut overflow_blobs = Vec::new();
    let entity_type = state.entity_type.clone();
    let entity_id = state.entity_id.clone();
    if let Some(obj) = state.fields.as_object_mut() {
        obj.insert(
            "Status".to_string(),
            serde_json::Value::String(state.status.clone()),
        );
        prune_transient_action_fields(&entity_type, obj);
        // Project action params into fields
        if let Some(p) = params.as_object() {
            for (k, v) in p {
                if is_transient_action_field(&entity_type, k) {
                    continue;
                }
                let field_meta = state_var_metadata.and_then(|m| m.get(k.as_str()));
                obj.insert(
                    k.clone(),
                    project_field_value(
                        k,
                        v,
                        mode,
                        &entity_type,
                        &entity_id,
                        field_meta,
                        &mut overflow_blobs,
                    ),
                );
            }
        }
        // Sync counters into fields
        for (k, v) in &state.counters {
            obj.insert(k.clone(), serde_json::Value::Number((*v as u64).into()));
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
            let field_meta = state_var_metadata.and_then(|m| m.get(k.as_str()));
            obj.insert(
                k.clone(),
                project_field_value(
                    k,
                    &serde_json::Value::Array(arr),
                    mode,
                    &entity_type,
                    &entity_id,
                    field_meta,
                    &mut overflow_blobs,
                ),
            );
        }
    }
    overflow_blobs
}

fn prune_transient_action_fields(
    entity_type: &str,
    fields: &mut serde_json::Map<String, serde_json::Value>,
) {
    if entity_type != "Repository" {
        return;
    }
    fields.remove("PackBytes");
    fields.remove("RefUpdates");
    fields.remove("ClientRequestId");
}

fn is_transient_action_field(entity_type: &str, field_name: &str) -> bool {
    entity_type == "Repository"
        && matches!(field_name, "PackBytes" | "RefUpdates" | "ClientRequestId")
}

pub(crate) fn prune_transient_action_fields_from_state(state: &mut EntityState) {
    if let Some(obj) = state.fields.as_object_mut() {
        prune_transient_action_fields(&state.entity_type, obj);
    }
}

fn project_field_value(
    field_name: &str,
    value: &serde_json::Value,
    mode: FieldSyncMode,
    entity_type: &str,
    entity_id: &str,
    field_meta: Option<&temper_jit::table::StateVarMetadata>,
    overflow_blobs: &mut Vec<OverflowBlobWrite>,
) -> serde_json::Value {
    let serialized = serde_json::to_vec(value).unwrap_or_else(|_| value.to_string().into_bytes());
    let serialized_len = serialized.len();
    // Per-field override wins over the mode default; mode default wins over
    // crate default (baked into FieldSyncMode::inline_max).
    let inline_max = field_meta
        .and_then(|m| m.overflow_inline_max_bytes)
        .unwrap_or_else(|| mode.inline_max());
    if serialized_len <= inline_max {
        return value.clone();
    }

    match mode {
        FieldSyncMode::InlineTruncate => {
            tracing::warn!(
                entity_type,
                entity_id,
                field = field_name,
                size_bytes = serialized_len,
                inline_max,
                "field truncated under InlineTruncate store — value replaced with placeholder; \
                 consider migrating this tenant to a Turso-backed store to preserve large values"
            );
            serde_json::Value::String(format!(
                "[truncated: {} bytes exceeds {} limit]",
                serialized_len, inline_max
            ))
        }
        FieldSyncMode::BlobRefs { .. } => {
            let digest = Sha256::digest(&serialized);
            let blob_key = format!("{FIELD_OVERFLOW_BLOB_PREFIX}{digest:x}.json");
            let ttl_seconds = field_meta.and_then(|m| m.overflow_ttl_seconds);
            if !overflow_blobs.iter().any(|blob| blob.key == blob_key) {
                overflow_blobs.push(OverflowBlobWrite {
                    key: blob_key.clone(),
                    body: serialized,
                    ttl_seconds,
                });
            }
            blob_ref_value(&blob_key, serialized_len)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_action_returns_scheduled_actions() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(42);

        let spec = r#"
[automaton]
name = "OAuthToken"
states = ["Active", "Refreshing", "Expired"]
initial = "Active"

[[action]]
name = "Activate"
from = ["Refreshing"]
to = "Active"
effect = [{ type = "schedule", action = "Refresh", delay_seconds = 2700 }]
"#;

        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state = EntityState {
            entity_type: "OAuthToken".into(),
            entity_id: "tok-1".into(),
            status: "Refreshing".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };

        let result = process_action(&mut state, &table, "Activate", &serde_json::json!({}));

        assert!(result.success, "action should succeed");
        assert_eq!(state.status, "Active");
        assert_eq!(result.scheduled_actions.len(), 1);
        assert_eq!(result.scheduled_actions[0].action, "Refresh");
        assert_eq!(result.scheduled_actions[0].delay_seconds, 2700);
    }

    /// Build a minimal `EntityState` for guard-error rendering tests.
    fn test_state(entity_type: &str, status: &str) -> EntityState {
        EntityState {
            entity_type: entity_type.into(),
            entity_id: "e-1".into(),
            status: status.into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn guard_failure_error_names_guard_and_field() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(7);
        // Cross-entity guard on `landing_file_id`: the ref is unresolved, so the
        // guard fails and the error must name the guard kind and the ref.
        let spec = r#"
[automaton]
name = "Doc"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "SubmitForReview"
from = ["Draft"]
to = "Submitted"
guard = [{ type = "cross_entity_state", entity_type = "File", entity_id_source = "landing_file_id", required_status = ["Ready", "Locked"] }]
"#;
        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state = test_state("Doc", "Draft");

        // No __xref boolean set -> guard fails.
        let result = process_action_with_xref(
            &mut state,
            &table,
            "SubmitForReview",
            &serde_json::json!({}),
            &std::collections::BTreeMap::new(),
        );

        assert!(!result.success);
        let error = result.error.expect("guard rejection must carry an error");
        assert!(
            error.contains("SubmitForReview")
                && error.contains("blocked from state 'Draft'")
                && error.contains("cross_entity_state")
                && error.contains("landing_file_id")
                && error.contains("Ready,Locked"),
            "expected specific guard error, got: {error}"
        );
    }

    #[test]
    fn from_state_miss_keeps_generic_error() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(7);
        let spec = r#"
[automaton]
name = "Doc"
states = ["Draft", "Submitted", "Closed"]
initial = "Draft"

[[action]]
name = "Close"
from = ["Submitted"]
to = "Closed"
"#;
        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state = test_state("Doc", "Draft");

        // Close only fires from Submitted; from Draft this is a from-state miss.
        let result = process_action(&mut state, &table, "Close", &serde_json::json!({}));

        assert!(!result.success);
        let error = result.error.expect("rejection must carry an error");
        assert_eq!(error, "Action 'Close' not valid from state 'Draft'");
    }

    #[test]
    fn test_apply_effects_returns_scheduled_actions_tuple() {
        let effects = vec![
            Effect::SetState("Active".into()),
            Effect::ScheduleAction {
                action: "Refresh".into(),
                delay_seconds: 3600,
            },
        ];

        let mut state = EntityState {
            entity_type: "Token".into(),
            entity_id: "t1".into(),
            status: "Idle".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };

        let (custom, scheduled, _spawns, _schedule_at) =
            apply_effects(&mut state, &effects, &serde_json::json!({}));

        assert!(custom.is_empty());
        assert_eq!(scheduled.len(), 1);
        assert_eq!(scheduled[0].action, "Refresh");
        assert_eq!(scheduled[0].delay_seconds, 3600);
        assert_eq!(state.status, "Active");
    }

    #[test]
    fn test_apply_effects_uses_numeric_param_amounts_for_counter_deltas() {
        let effects = vec![
            Effect::IncrementCounterByParam {
                var: "used_bytes".into(),
                param: "size_bytes".into(),
            },
            Effect::DecrementCounterByParam {
                var: "used_bytes".into(),
                param: "released_bytes".into(),
            },
        ];

        let mut state = EntityState {
            entity_type: "Workspace".into(),
            entity_id: "ws-1".into(),
            status: "Active".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::from([("used_bytes".into(), 10usize)]),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };

        let (_custom, _scheduled, _spawns, _schedule_at) = apply_effects(
            &mut state,
            &effects,
            &serde_json::json!({
                "size_bytes": "30",
                "released_bytes": 7,
            }),
        );

        assert_eq!(state.counters.get("used_bytes"), Some(&33));
    }

    #[test]
    fn test_spawn_effect_collects_requests() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(42);

        let spec = r#"
[automaton]
name = "LeadAgent"
states = ["Ready", "Planning"]
initial = "Ready"

[[action]]
name = "StartPlan"
from = ["Ready"]
to = "Planning"
effect = [
    { type = "spawn", entity_type = "TestWorkflow", entity_id_source = "{uuid}", initial_action = "Start", store_id_in = "test_wf_id" },
]
"#;

        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state = EntityState {
            entity_type: "LeadAgent".into(),
            entity_id: "agent-1".into(),
            status: "Ready".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };

        let result = process_action(&mut state, &table, "StartPlan", &serde_json::json!({}));

        assert!(result.success, "action should succeed");
        assert_eq!(state.status, "Planning");
        assert_eq!(result.spawn_requests.len(), 1);
        assert_eq!(result.spawn_requests[0].entity_type, "TestWorkflow");
        assert_eq!(
            result.spawn_requests[0].initial_action.as_deref(),
            Some("Start")
        );
        assert_eq!(
            result.spawn_requests[0].store_id_in.as_deref(),
            Some("test_wf_id")
        );

        // Child ID should be stored in parent fields
        assert!(
            state.fields.get("test_wf_id").is_some(),
            "child ID should be stored in parent fields"
        );
    }

    #[test]
    fn test_cross_entity_guard_with_xref() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(42);

        let spec = r#"
[automaton]
name = "LeadAgent"
states = ["Planning", "Deployed"]
initial = "Planning"

[[action]]
name = "Promote"
from = ["Planning"]
to = "Deployed"
guard = [
    { type = "cross_entity_state", entity_type = "TestWorkflow", entity_id_source = "test_wf_id", required_status = ["Passed"] }
]
"#;

        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state = EntityState {
            entity_type: "LeadAgent".into(),
            entity_id: "agent-1".into(),
            status: "Planning".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({"test_wf_id": "wf-1"}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };

        // Without cross-entity booleans, guard should fail
        let result = process_action(&mut state, &table, "Promote", &serde_json::json!({}));
        assert!(
            !result.success,
            "should fail without cross-entity resolution"
        );

        // With cross-entity booleans via process_action_with_xref
        let mut xref = std::collections::BTreeMap::new();
        xref.insert("__xref:TestWorkflow:test_wf_id".to_string(), true);
        let result =
            process_action_with_xref(&mut state, &table, "Promote", &serde_json::json!({}), &xref);
        assert!(
            result.success,
            "should succeed with cross-entity boolean = true"
        );
        assert_eq!(state.status, "Deployed");
    }

    #[test]
    fn cross_entity_budget_covers_multi_entity_guard_contract() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(42);

        let spec = r#"
[automaton]
name = "ReviewPacket"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "Submit"
from = ["Draft"]
to = "Submitted"
guard = [
    { type = "cross_entity_state", entity_type = "Attachment", entity_id_source = "attachment_a_id", required_status = ["Ready", "Locked"] },
    { type = "cross_entity_state", entity_type = "Attachment", entity_id_source = "attachment_b_id", required_status = ["Ready", "Locked"] },
    { type = "cross_entity_state", entity_type = "Attachment", entity_id_source = "attachment_c_id", required_status = ["Ready", "Locked"] },
    { type = "cross_entity_state", entity_type = "Attachment", entity_id_source = "attachment_d_id", required_status = ["Ready", "Locked"] },
    { type = "cross_entity_state", entity_type = "Attachment", entity_id_source = "attachment_e_id", required_status = ["Ready", "Locked"] },
    { type = "cross_entity_state", entity_type = "Attachment", entity_id_source = "attachment_f_id", required_status = ["Ready", "Locked"] },
    { type = "cross_entity_state", entity_type = "Attachment", entity_id_source = "attachment_g_id", required_status = ["Ready", "Locked"] },
    { type = "cross_entity_state", entity_type = "Attachment", entity_id_source = "attachment_h_id", required_status = ["Ready", "Locked"] }
]
"#;

        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state = EntityState {
            entity_type: "ReviewPacket".into(),
            entity_id: "rp-1".into(),
            status: "Draft".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({
                "attachment_a_id": "att-1",
                "attachment_b_id": "att-2",
                "attachment_c_id": "att-3",
                "attachment_d_id": "att-4",
                "attachment_e_id": "att-5",
                "attachment_f_id": "att-6",
                "attachment_g_id": "att-7",
                "attachment_h_id": "att-8"
            }),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };
        let xref = std::collections::BTreeMap::from([
            ("__xref:Attachment:attachment_a_id".to_string(), true),
            ("__xref:Attachment:attachment_b_id".to_string(), true),
            ("__xref:Attachment:attachment_c_id".to_string(), true),
            ("__xref:Attachment:attachment_d_id".to_string(), true),
            ("__xref:Attachment:attachment_e_id".to_string(), true),
            ("__xref:Attachment:attachment_f_id".to_string(), true),
            ("__xref:Attachment:attachment_g_id".to_string(), true),
            ("__xref:Attachment:attachment_h_id".to_string(), true),
        ]);
        let required_guard_lookup_count = xref.len();
        assert!(
            MAX_CROSS_ENTITY_LOOKUPS >= required_guard_lookup_count,
            "multi-entity guards need at least {required_guard_lookup_count} cross-entity lookups"
        );

        let result =
            process_action_with_xref(&mut state, &table, "Submit", &serde_json::json!({}), &xref);

        assert!(
            result.success,
            "eight resolved cross-entity guards should allow Submit"
        );
        assert_eq!(state.status, "Submitted");
    }

    #[test]
    fn test_schedule_at_resolves_from_field_after_sync() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(42);

        let spec = r#"
[automaton]
name = "CronJob"
states = ["Active"]
initial = "Active"

[[state]]
name = "next_run_at"
type = "string"
initial = ""

[[action]]
name = "TriggerComplete"
from = ["Active"]
params = ["next_run_at"]
effect = [{ type = "schedule_at", field = "next_run_at", action = "Trigger" }]
"#;

        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state = EntityState {
            entity_type: "CronJob".into(),
            entity_id: "cron-1".into(),
            status: "Active".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };

        // Provide next_run_at as a param (simulates WASM callback)
        let future_time = sim_now() + chrono::Duration::seconds(300);
        let future_iso = future_time.to_rfc3339();
        let params = serde_json::json!({ "next_run_at": future_iso });

        let result = process_action(&mut state, &table, "TriggerComplete", &params);

        assert!(result.success, "action should succeed");
        assert_eq!(
            result.scheduled_actions.len(),
            1,
            "should have one scheduled action"
        );
        assert_eq!(result.scheduled_actions[0].action, "Trigger");
        // Delay should be ~300 seconds (sim_now is deterministic)
        assert_eq!(result.scheduled_actions[0].delay_seconds, 300);
    }

    #[test]
    fn test_schedule_at_past_timestamp_fires_immediately() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(42);

        let spec = r#"
[automaton]
name = "CronJob"
states = ["Active"]
initial = "Active"

[[state]]
name = "next_run_at"
type = "string"
initial = ""

[[action]]
name = "TriggerComplete"
from = ["Active"]
params = ["next_run_at"]
effect = [{ type = "schedule_at", field = "next_run_at", action = "Trigger" }]
"#;

        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state = EntityState {
            entity_type: "CronJob".into(),
            entity_id: "cron-1".into(),
            status: "Active".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };

        // Provide a timestamp in the past
        let past_time = sim_now() - chrono::Duration::seconds(60);
        let past_iso = past_time.to_rfc3339();
        let params = serde_json::json!({ "next_run_at": past_iso });

        let result = process_action(&mut state, &table, "TriggerComplete", &params);

        assert!(result.success);
        assert_eq!(result.scheduled_actions.len(), 1);
        assert_eq!(
            result.scheduled_actions[0].delay_seconds, 0,
            "past timestamp should fire immediately"
        );
    }

    #[test]
    fn test_schedule_at_missing_field_skips() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(42);

        let spec = r#"
[automaton]
name = "CronJob"
states = ["Active"]
initial = "Active"

[[action]]
name = "Complete"
from = ["Active"]
effect = [{ type = "schedule_at", field = "next_run_at", action = "Trigger" }]
"#;

        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state = EntityState {
            entity_type: "CronJob".into(),
            entity_id: "cron-1".into(),
            status: "Active".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };

        let result = process_action(&mut state, &table, "Complete", &serde_json::json!({}));

        assert!(result.success);
        assert!(
            result.scheduled_actions.is_empty(),
            "missing field should produce no scheduled actions"
        );
    }

    #[test]
    fn test_set_counter_from_param_effect_sets_counter_and_field() {
        let spec = r#"
[automaton]
name = "Upload"
states = ["Pending", "Ready"]
initial = "Pending"

[[state]]
name = "size_bytes"
type = "counter"
initial = "0"

[[action]]
name = "Complete"
from = ["Pending"]
to = "Ready"
params = ["payload_size"]
effect = [{ type = "set_counter_from_param", var = "size_bytes", param = "payload_size" }]
"#;

        let table = TransitionTable::from_ioa_source(spec);
        let mut state = EntityState {
            entity_type: "Upload".into(),
            entity_id: "upload-1".into(),
            status: "Pending".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };

        let result = process_action(
            &mut state,
            &table,
            "Complete",
            &serde_json::json!({ "payload_size": 4096 }),
        );

        assert!(result.success, "action should succeed");
        assert_eq!(state.status, "Ready");
        assert_eq!(state.counters.get("size_bytes"), Some(&4096));
        assert_eq!(
            state.fields.get("size_bytes").and_then(|v| v.as_u64()),
            Some(4096)
        );
    }

    #[test]
    fn test_spawn_with_copy_fields() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(42);

        let spec = r#"
[automaton]
name = "Agent"
states = ["Ready", "Spawning"]
initial = "Ready"

[[state]]
name = "system_prompt"
type = "string"
initial = ""

[[state]]
name = "model"
type = "string"
initial = ""

[[action]]
name = "Launch"
from = ["Ready"]
to = "Spawning"
effect = [
    { type = "spawn", entity_type = "Session", entity_id_source = "{uuid}", initial_action = "Configure", store_id_in = "last_session_id", copy_fields = "system_prompt,model" },
]
"#;

        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state = EntityState {
            entity_type: "Agent".into(),
            entity_id: "agent-1".into(),
            status: "Ready".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({
                "system_prompt": "You are a helpful assistant",
                "model": "claude-3-opus"
            }),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        };

        let result = process_action(&mut state, &table, "Launch", &serde_json::json!({}));

        assert!(result.success, "action should succeed");
        assert_eq!(result.spawn_requests.len(), 1);

        let req = &result.spawn_requests[0];
        assert_eq!(req.entity_type, "Session");
        assert_eq!(
            req.copy_fields.as_ref().unwrap(),
            &vec!["system_prompt".to_string(), "model".to_string()]
        );
        assert_eq!(
            req.copied_field_values.get("system_prompt").unwrap(),
            "You are a helpful assistant"
        );
        assert_eq!(
            req.copied_field_values.get("model").unwrap(),
            "claude-3-opus"
        );
    }

    fn make_state(entity_type: &str, entity_id: &str) -> EntityState {
        EntityState {
            entity_type: entity_type.into(),
            entity_id: entity_id.into(),
            status: "Initial".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: serde_json::json!({}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        }
    }

    fn ref_transition_table() -> TransitionTable {
        let spec = r#"
[automaton]
name = "Ref"
states = ["Active", "Deleted"]
initial = "Active"

[[action]]
name = "Update"
kind = "input"
from = ["Active"]
to = "Active"
params = ["PreviousCommitSha", "NewCommitSha"]

[[action]]
name = "ForceUpdate"
kind = "input"
from = ["Active"]
to = "Active"
params = ["NewCommitSha"]
"#;

        TransitionTable::from_ioa_source(spec)
    }

    fn active_ref_state(target: &str) -> EntityState {
        let mut state = make_state("Ref", "rf-repo-refs-heads-main");
        state.status = "Active".to_string();
        state.fields = serde_json::json!({ "TargetCommitSha": target });
        state
    }

    #[test]
    fn ref_update_advances_target_commit_sha_after_matching_cas() {
        let table = ref_transition_table();
        let current = "1111111111111111111111111111111111111111";
        let next = "2222222222222222222222222222222222222222";
        let mut state = active_ref_state(current);

        let result = process_action(
            &mut state,
            &table,
            "Update",
            &serde_json::json!({
                "PreviousCommitSha": current,
                "NewCommitSha": next
            }),
        );

        assert!(result.success, "matching CAS update should succeed");
        assert_eq!(
            state.fields.get("TargetCommitSha").and_then(|v| v.as_str()),
            Some(next)
        );
        assert_eq!(
            result
                .event
                .as_ref()
                .and_then(|event| event.params.get("TargetCommitSha"))
                .and_then(|value| value.as_str()),
            Some(next)
        );
    }

    #[test]
    fn ref_update_rejects_stale_previous_commit_sha() {
        let table = ref_transition_table();
        let current = "1111111111111111111111111111111111111111";
        let stale = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let next = "2222222222222222222222222222222222222222";
        let mut state = active_ref_state(current);

        let result = process_action(
            &mut state,
            &table,
            "Update",
            &serde_json::json!({
                "PreviousCommitSha": stale,
                "NewCommitSha": next
            }),
        );

        assert!(!result.success, "stale CAS update should fail");
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("stale ref")),
            "unexpected error: {:?}",
            result.error
        );
        assert_eq!(
            state.fields.get("TargetCommitSha").and_then(|v| v.as_str()),
            Some(current)
        );
    }

    #[test]
    fn ref_force_update_advances_target_commit_sha() {
        let table = ref_transition_table();
        let current = "1111111111111111111111111111111111111111";
        let next = "2222222222222222222222222222222222222222";
        let mut state = active_ref_state(current);

        let result = process_action(
            &mut state,
            &table,
            "ForceUpdate",
            &serde_json::json!({
                "NewCommitSha": next
            }),
        );

        assert!(result.success, "force update should succeed");
        assert_eq!(
            state.fields.get("TargetCommitSha").and_then(|v| v.as_str()),
            Some(next)
        );
    }

    #[test]
    fn default_field_inline_max_is_128kb() {
        assert_eq!(DEFAULT_FIELD_INLINE_MAX, 131_072);
    }

    #[test]
    fn blob_refs_default_carries_default_ceiling() {
        let mode = FieldSyncMode::blob_refs_default();
        assert_eq!(
            mode,
            FieldSyncMode::BlobRefs {
                default_inline_max: DEFAULT_FIELD_INLINE_MAX
            }
        );
    }

    #[test]
    fn field_under_ceiling_stays_inline_blob_refs() {
        let mut state = make_state("Session", "s-1");
        let under = "x".repeat(64 * 1024); // 64 KB, under 128 KB ceiling
        let params = serde_json::json!({ "user_message": under });

        let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());

        assert!(
            overflow.is_empty(),
            "no blob overflow for field under ceiling"
        );
        assert_eq!(
            state
                .fields
                .get("user_message")
                .and_then(|v| v.as_str())
                .map(str::len),
            Some(64 * 1024),
            "inline value preserved"
        );
    }

    #[test]
    fn field_over_default_ceiling_overflows_to_blob() {
        let mut state = make_state("Session", "s-1");
        let over = "y".repeat(200 * 1024); // 200 KB, over 128 KB ceiling
        let params = serde_json::json!({ "user_message": over });

        let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());

        assert_eq!(overflow.len(), 1, "one overflow blob written");
        let ref_obj = state
            .fields
            .get("user_message")
            .and_then(|v| v.as_object())
            .expect("blob ref object present");
        assert!(ref_obj.contains_key(crate::blobs::FIELD_OVERFLOW_REF_KEY));
        assert!(ref_obj.contains_key(crate::blobs::FIELD_OVERFLOW_SIZE_KEY));
    }

    #[test]
    fn field_over_legacy_32k_stays_inline_under_new_ceiling() {
        // Regression test for ADR-0045: fields in the 32KB-128KB band that
        // previously overflowed now stay inline.
        let mut state = make_state("Session", "s-1");
        let mid = "z".repeat(80 * 1024); // 80 KB — above old 32KB cap, below new 128KB
        let params = serde_json::json!({ "mid_field": mid });

        let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());

        assert!(
            overflow.is_empty(),
            "80KB field stays inline under new ceiling"
        );
        assert_eq!(
            state
                .fields
                .get("mid_field")
                .and_then(|v| v.as_str())
                .map(str::len),
            Some(80 * 1024)
        );
    }

    #[test]
    fn inline_truncate_mode_truncates_and_warns_above_ceiling() {
        let mut state = make_state("Session", "s-1");
        let huge = "q".repeat(200 * 1024);
        let params = serde_json::json!({ "user_message": huge });

        let overflow = sync_fields(&mut state, &params, FieldSyncMode::InlineTruncate);

        assert!(overflow.is_empty(), "InlineTruncate never writes blobs");
        let v = state
            .fields
            .get("user_message")
            .and_then(|v| v.as_str())
            .expect("truncation produces a string placeholder");
        assert!(v.starts_with("[truncated:"), "placeholder shape preserved");
    }

    #[test]
    fn repository_receive_pack_fields_are_transient() {
        let mut state = make_state("Repository", "rp-acme-app");
        state.fields = serde_json::json!({
            "OwnerAccountId": "acme",
            "Name": "app",
            "DefaultBranch": "main",
            "PackBytes": "stale-pack",
            "RefUpdates": [{"Name": "refs/heads/main"}],
            "ClientRequestId": "stale-request"
        });
        let params = serde_json::json!({
            "PackBytes": "fresh-pack",
            "RefUpdates": [{"Name": "refs/heads/main", "NewCommitSha": "abc"}],
            "ClientRequestId": "fresh-request"
        });

        let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());

        assert!(overflow.is_empty());
        assert!(state.fields.get("PackBytes").is_none());
        assert!(state.fields.get("RefUpdates").is_none());
        assert!(state.fields.get("ClientRequestId").is_none());
        assert_eq!(
            state.fields.get("OwnerAccountId").and_then(|v| v.as_str()),
            Some("acme")
        );
    }

    #[test]
    fn custom_inline_max_overrides_default() {
        // A caller constructing BlobRefs with a non-default ceiling must see
        // that ceiling applied, not the crate default.
        let mut state = make_state("Session", "s-1");
        let mid = "m".repeat(50 * 1024); // 50 KB
        let params = serde_json::json!({ "mid_field": mid });

        let tight = FieldSyncMode::BlobRefs {
            default_inline_max: 32 * 1024, // 32 KB — tighter than default
        };
        let overflow = sync_fields(&mut state, &params, tight);

        assert_eq!(overflow.len(), 1, "50KB overflows under 32KB ceiling");
    }

    #[test]
    fn oversize_list_field_also_overflows_to_blob() {
        // Regression guard: sync_fields threads project_field_value through the
        // lists loop as well as the params loop. Both branches must respect
        // the ceiling.
        let mut state = make_state("Session", "s-1");
        let big = "L".repeat(10 * 1024);
        state.lists.insert(
            "tool_outputs".to_string(),
            (0..16).map(|_| big.clone()).collect(), // 160 KB serialized
        );

        let overflow = sync_fields(
            &mut state,
            &serde_json::json!({}),
            FieldSyncMode::blob_refs_default(),
        );

        assert_eq!(overflow.len(), 1, "oversize list overflows to blob");
        let ref_obj = state
            .fields
            .get("tool_outputs")
            .and_then(|v| v.as_object())
            .expect("blob ref object present for list field");
        assert!(ref_obj.contains_key(crate::blobs::FIELD_OVERFLOW_REF_KEY));
    }

    #[test]
    fn duplicate_oversize_value_produces_single_blob_write() {
        // Content-addressed dedupe: two params with identical oversize content
        // share one blob write.
        let mut state = make_state("Session", "s-1");
        let big = "d".repeat(200 * 1024);
        let params = serde_json::json!({ "a": &big, "b": &big });

        let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());

        assert_eq!(overflow.len(), 1, "dedupe by content hash");
    }

    /// ARN-247: an action that declares a *subset* of the entity's state vars
    /// must not project undeclared request-body keys into entity fields. Models
    /// the ARN-245 case: `AttachVector` (declares only the two vector params),
    /// enabled from the terminal-ish `Done` state, must leave a closed
    /// WorkSummary's `goal`/`outcome` frozen even if the caller smuggles them
    /// into the body. Red before the fix (undeclared keys overwrite fields);
    /// green after (they are dropped at the runtime action boundary).
    #[test]
    fn declared_params_only_undeclared_body_keys_do_not_mutate_fields() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(247);

        let spec = r#"
[automaton]
name = "WorkSummary"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "goal"
type = "string"
initial = ""

[[state]]
name = "outcome"
type = "string"
initial = ""

[[state]]
name = "semantic_vector"
type = "string"
initial = ""

[[state]]
name = "semantic_vector_model"
type = "string"
initial = ""

[[action]]
name = "AttachVector"
kind = "input"
from = ["Done"]
to = "Done"
params = ["semantic_vector", "semantic_vector_model"]
"#;

        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state = make_state("WorkSummary", "g");
        state.status = "Done".into();
        state.fields = serde_json::json!({
            "goal": "original goal",
            "outcome": "shipped honestly",
        });

        // Caller smuggles undeclared `goal`/`outcome` alongside the two declared params.
        let params = serde_json::json!({
            "semantic_vector": "[0.1,0.2]",
            "semantic_vector_model": "text-embedding-3-small",
            "goal": "HIJACKED",
            "outcome": "REWRITTEN",
        });

        let result = process_action(&mut state, &table, "AttachVector", &params);
        assert!(result.success, "AttachVector should succeed from Done");

        // Declared params ARE written.
        assert_eq!(
            state.fields.get("semantic_vector").and_then(|v| v.as_str()),
            Some("[0.1,0.2]"),
            "declared param semantic_vector must be projected",
        );
        assert_eq!(
            state
                .fields
                .get("semantic_vector_model")
                .and_then(|v| v.as_str()),
            Some("text-embedding-3-small"),
            "declared param semantic_vector_model must be projected",
        );

        // Undeclared params must NOT overwrite the closed record's fields.
        assert_eq!(
            state.fields.get("goal").and_then(|v| v.as_str()),
            Some("original goal"),
            "ARN-247: undeclared `goal` must not overwrite a closed WorkSummary field",
        );
        assert_eq!(
            state.fields.get("outcome").and_then(|v| v.as_str()),
            Some("shipped honestly"),
            "ARN-247: undeclared `outcome` must not overwrite a closed WorkSummary field",
        );
    }

    fn work_summary_table() -> TransitionTable {
        let spec = r#"
[automaton]
name = "WorkSummary"
states = ["Open", "Done"]
initial = "Open"

[[action]]
name = "AttachVector"
kind = "input"
from = ["Done"]
to = "Done"
params = ["semantic_vector", "semantic_vector_model"]
"#;
        TransitionTable::from_ioa_source(spec)
    }

    #[test]
    fn restrict_to_declared_params_drops_undeclared_keeps_declared() {
        let table = work_summary_table();
        let params = serde_json::json!({
            "semantic_vector": "v",
            "semantic_vector_model": "m",
            "goal": "smuggled",
        });
        let restricted =
            restrict_to_declared_params(&table, "AttachVector", "WorkSummary", &params);
        let obj = restricted.as_object().expect("object");
        assert!(obj.contains_key("semantic_vector"), "declared kept");
        assert!(obj.contains_key("semantic_vector_model"), "declared kept");
        assert!(!obj.contains_key("goal"), "undeclared dropped");
    }

    #[test]
    fn restrict_to_declared_params_passes_through_unknown_action() {
        // Action absent from the table's declaration map -> no restriction, so
        // synthetic/kernel actions and older deserialized tables keep behavior.
        let table = work_summary_table();
        let params = serde_json::json!({ "anything": "kept" });
        let restricted =
            restrict_to_declared_params(&table, "NotADeclaredAction", "WorkSummary", &params);
        assert_eq!(
            restricted.as_ref(),
            &params,
            "unknown action passes through"
        );
    }

    #[test]
    fn restrict_to_declared_params_keeps_transient_repository_fields() {
        // Transient action fields are consumed by triggers but never persisted;
        // they must survive the filter even when this action declares none.
        let spec = r#"
[automaton]
name = "Repository"
states = ["Active"]
initial = "Active"

[[action]]
name = "IngestPackNoDecl"
kind = "input"
from = ["Active"]
to = "Active"
"#;
        let table = TransitionTable::from_ioa_source(spec);
        let params = serde_json::json!({ "PackBytes": "base64", "Bogus": "drop-me" });
        let restricted =
            restrict_to_declared_params(&table, "IngestPackNoDecl", "Repository", &params);
        let obj = restricted.as_object().expect("object");
        assert!(obj.contains_key("PackBytes"), "transient field preserved");
        assert!(
            !obj.contains_key("Bogus"),
            "undeclared non-transient dropped"
        );
    }

    #[test]
    fn restrict_to_declared_params_matches_snake_declared_against_pascal_body() {
        // paw-fs declares snake_case logical params but dispatches with the
        // PascalCase field names it stores; they must be treated as the same
        // declared field, while a genuinely undeclared key is still dropped.
        let spec = r#"
[automaton]
name = "Directory"
states = ["Active"]
initial = "Active"

[[action]]
name = "Create"
kind = "input"
from = ["Active"]
to = "Active"
params = ["name", "path", "parent_id", "workspace_id"]
"#;
        let table = TransitionTable::from_ioa_source(spec);
        let params = serde_json::json!({
            "Name": "/",
            "Path": "/",
            "WorkspaceId": "wsA",
            "ParentId": "d-1",
            "Smuggled": "drop-me",
        });
        let restricted = restrict_to_declared_params(&table, "Create", "Directory", &params);
        let obj = restricted.as_object().expect("object");
        assert!(obj.contains_key("Name"), "PascalCase declared kept as sent");
        assert!(obj.contains_key("WorkspaceId"), "snake<->Pascal match kept");
        assert!(obj.contains_key("ParentId"), "snake<->Pascal match kept");
        assert!(!obj.contains_key("Smuggled"), "truly undeclared dropped");
        // And the reject boundary agrees: only the smuggled key is undeclared.
        assert_eq!(
            undeclared_param_keys(&table, "Create", "Directory", &params),
            vec!["Smuggled".to_string()],
        );
    }

    #[test]
    fn undeclared_param_keys_lists_only_undeclared() {
        let table = work_summary_table();
        let body = serde_json::json!({
            "semantic_vector": "v",
            "goal": "x",
            "outcome": "y",
        });
        let mut keys = undeclared_param_keys(&table, "AttachVector", "WorkSummary", &body);
        keys.sort();
        assert_eq!(keys, vec!["goal".to_string(), "outcome".to_string()]);
    }

    #[test]
    fn undeclared_param_keys_empty_for_unknown_action() {
        // No declaration -> nothing to reject (the boundary must not 400).
        let table = work_summary_table();
        let body = serde_json::json!({ "whatever": "1" });
        assert!(
            undeclared_param_keys(&table, "GhostAction", "WorkSummary", &body).is_empty(),
            "unknown action yields no rejectable keys",
        );
    }
}
