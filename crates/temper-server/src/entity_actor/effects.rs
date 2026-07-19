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

/// Effects that require follow-up processing after state mutation completes.
pub type AppliedEffects = (
    Vec<String>,
    Vec<ScheduledAction>,
    Vec<SpawnRequest>,
    Vec<ScheduleAtRequest>,
);

/// Maximum cross-entity lookups per transition (TigerStyle budget).
///
/// Rich contracts may legitimately verify several sibling entities in one
/// guarded transition.
pub const MAX_CROSS_ENTITY_LOOKUPS: usize = 16;
/// Maximum entity spawns per transition (TigerStyle budget).
pub const MAX_SPAWNS_PER_TRANSITION: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RuntimeFieldKind {
    Counter,
    String,
}

fn runtime_field_kind(table: &TransitionTable, name: &str) -> Option<RuntimeFieldKind> {
    table
        .runtime_invariants
        .iter()
        .find_map(|invariant| runtime_assert_field_kind(&invariant.assertion, name))
}

fn runtime_assert_field_kind(
    assertion: &temper_spec::automaton::RuntimeAssert,
    name: &str,
) -> Option<RuntimeFieldKind> {
    use temper_spec::automaton::RuntimeAssert;
    match assertion {
        RuntimeAssert::StringNonEmpty { var } if var == name => Some(RuntimeFieldKind::String),
        RuntimeAssert::CounterPositive { var } | RuntimeAssert::CounterCompare { var, .. }
            if var == name =>
        {
            Some(RuntimeFieldKind::Counter)
        }
        RuntimeAssert::CounterVarCompare { left, right, .. } if left == name || right == name => {
            Some(RuntimeFieldKind::Counter)
        }
        RuntimeAssert::And(parts) | RuntimeAssert::Or(parts) => parts
            .iter()
            .find_map(|part| runtime_assert_field_kind(part, name)),
        _ => None,
    }
}

/// Build the canonical initial entity state shared by production, simulation,
/// and the durable data-only creation path.
pub(crate) fn build_initial_entity_state(
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    initial_fields: &serde_json::Value,
) -> Result<EntityState, String> {
    use temper_jit::table::types::StateVarInitialValue;

    let mut counters = std::collections::BTreeMap::new();
    let mut booleans = std::collections::BTreeMap::new();
    let mut declared_fields = serde_json::Map::new();
    for (name, initial) in &table.state_var_initials {
        match initial {
            StateVarInitialValue::Counter(value) => {
                counters.insert(name.clone(), *value);
                declared_fields.insert(name.clone(), serde_json::json!(value));
            }
            StateVarInitialValue::Bool(value) => {
                booleans.insert(name.clone(), *value);
                declared_fields.insert(name.clone(), serde_json::Value::Bool(*value));
            }
            StateVarInitialValue::String(value) => {
                declared_fields.insert(name.clone(), serde_json::Value::String(value.clone()));
            }
        }
    }

    let fields = if let Some(initial) = initial_fields.as_object() {
        for (name, value) in initial {
            match table.state_var_initials.get(name) {
                Some(StateVarInitialValue::Counter(_)) => {
                    match value.as_u64().and_then(|value| usize::try_from(value).ok()) {
                        Some(value) => {
                            if !table.model_protected_state_vars.contains(name) {
                                counters.insert(name.clone(), value);
                            }
                        }
                        None if runtime_field_kind(table, name)
                            == Some(RuntimeFieldKind::Counter) =>
                        {
                            return Err(format!("runtime counter field '{name}' must be a usize"));
                        }
                        None => {}
                    }
                }
                Some(StateVarInitialValue::Bool(_)) => {
                    let value = value
                        .as_bool()
                        .ok_or_else(|| format!("declared bool field '{name}' must be boolean"))?;
                    if !table.model_protected_state_vars.contains(name) {
                        booleans.insert(name.clone(), value);
                    }
                }
                Some(StateVarInitialValue::String(_)) => {
                    if value.as_str().is_none()
                        && runtime_field_kind(table, name) == Some(RuntimeFieldKind::String)
                    {
                        return Err(format!("runtime string field '{name}' must be a string"));
                    }
                }
                _ => {}
            }
            if table.model_protected_state_vars.contains(name) {
                // Model-proved counters and booleans start from the declared
                // initial state. Caller fields remain available to a modeled
                // Create action, but cannot rewrite logical state directly.
                continue;
            }
            declared_fields.insert(name.clone(), value.clone());
        }
        declared_fields
            .entry("Id".to_string())
            .or_insert_with(|| serde_json::Value::String(entity_id.to_string()));
        declared_fields
            .entry("Status".to_string())
            .or_insert_with(|| serde_json::Value::String(table.initial_state.clone()));
        serde_json::Value::Object(declared_fields)
    } else {
        return Err("initial entity fields must be an object".to_string());
    };

    let item_count = counters.get("items").copied().unwrap_or(0);
    Ok(EntityState {
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
        status: table.initial_state.clone(),
        item_count,
        counters,
        booleans,
        lists: std::collections::BTreeMap::new(),
        fields,
        events: std::collections::VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
    })
}

/// Synchronize direct field mutations into declared counter/boolean state.
pub(crate) fn sync_declared_state_vars_from_fields(
    state: &mut EntityState,
    table: &TransitionTable,
) -> Result<(), String> {
    let fields = state
        .fields
        .as_object()
        .ok_or_else(|| "entity fields must be an object".to_string())?;
    sync_declared_state_vars(
        &mut state.counters,
        &mut state.booleans,
        &mut state.item_count,
        table,
        fields,
        DeclaredStateSyncMode::DirectFields,
    )?;
    if let Some(fields) = state.fields.as_object_mut() {
        for name in &table.model_protected_state_vars {
            if let Some(value) = state.counters.get(name) {
                fields.insert(name.clone(), serde_json::json!(value));
            } else if let Some(value) = state.booleans.get(name) {
                fields.insert(name.clone(), serde_json::json!(value));
            }
        }
    }
    Ok(())
}

/// Synchronize action parameters into declared counter/boolean state before
/// applying effects, so effect mutations remain authoritative.
pub(crate) fn sync_declared_state_vars_from_params(
    state: &mut EntityState,
    table: &TransitionTable,
    params: &serde_json::Value,
) -> Result<(), String> {
    let params = params
        .as_object()
        .ok_or_else(|| "action params must be an object".to_string())?;
    sync_declared_state_vars(
        &mut state.counters,
        &mut state.booleans,
        &mut state.item_count,
        table,
        params,
        DeclaredStateSyncMode::ActionParams,
    )
}

#[derive(Clone, Copy)]
enum DeclaredStateSyncMode {
    ActionParams,
    DirectFields,
}

fn sync_declared_state_vars(
    counters: &mut std::collections::BTreeMap<String, usize>,
    booleans: &mut std::collections::BTreeMap<String, bool>,
    item_count: &mut usize,
    table: &TransitionTable,
    values: &serde_json::Map<String, serde_json::Value>,
    mode: DeclaredStateSyncMode,
) -> Result<(), String> {
    use temper_jit::table::types::StateVarInitialValue;

    for (name, initial) in &table.state_var_initials {
        let Some(value) = values.get(name) else {
            continue;
        };
        if table.model_protected_state_vars.contains(name) {
            if matches!(mode, DeclaredStateSyncMode::ActionParams) {
                // Preserve the parameter as an input to explicit modeled
                // effects, but never treat its name as an implicit assignment.
                continue;
            }
            let changed = match initial {
                StateVarInitialValue::Counter(_) => {
                    value.as_u64().and_then(|value| usize::try_from(value).ok())
                        != counters.get(name).copied()
                }
                StateVarInitialValue::Bool(_) => value.as_bool() != booleans.get(name).copied(),
                StateVarInitialValue::String(_) => false,
            };
            if changed {
                return Err(format!(
                    "declared state field '{name}' is controlled by verified transition effects"
                ));
            }
            continue;
        }
        match initial {
            StateVarInitialValue::Counter(_) => {
                let counter = value.as_u64().and_then(|value| usize::try_from(value).ok());
                let Some(counter) = counter else {
                    if runtime_field_kind(table, name) == Some(RuntimeFieldKind::Counter) {
                        return Err(format!("runtime counter field '{name}' must be a usize"));
                    }
                    continue;
                };
                counters.insert(name.clone(), counter);
                if name == "items" {
                    *item_count = counter;
                }
            }
            StateVarInitialValue::Bool(_) => {
                let boolean = value
                    .as_bool()
                    .ok_or_else(|| format!("declared bool field '{name}' must be boolean"))?;
                booleans.insert(name.clone(), boolean);
            }
            StateVarInitialValue::String(_) => {}
        }
    }
    Ok(())
}

/// Validate caller-supplied values for declared runtime state variables.
///
/// Internal overflow blob envelopes are allowed in stored state, but callers
/// cannot submit them in creation, action, replay, or direct-update payloads.
pub(crate) fn validate_declared_field_values(
    values: &serde_json::Value,
    table: &TransitionTable,
) -> Result<(), String> {
    let values = values
        .as_object()
        .ok_or_else(|| "declared field payload must be an object".to_string())?;
    for (name, value) in values {
        match runtime_field_kind(table, name) {
            Some(RuntimeFieldKind::Counter) => {
                if value
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .is_none()
                {
                    return Err(format!("runtime counter field '{name}' must be a usize"));
                }
            }
            Some(RuntimeFieldKind::String) if value.as_str().is_none() => {
                return Err(format!("runtime string field '{name}' must be a string"));
            }
            _ => {}
        }
    }
    Ok(())
}

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
    let state_before_action = state.clone();
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

            if let Err(error) = validate_declared_field_values(params, table) {
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

            if let Err(error) = sync_declared_state_vars_from_params(state, table, params) {
                *state = state_before_action;
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

            let (custom_effects, scheduled_actions, spawn_requests, schedule_at_requests) =
                match apply_effects(state, &transition_result.effects, params) {
                    Ok(result) => result,
                    Err(error) => {
                        *state = state_before_action;
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
                };
            apply_new_state_fallback(state, &from_status, &to_status);
            let overflow_blobs = sync_fields_with_metadata(
                state,
                params,
                field_sync_mode,
                Some(&table.state_var_metadata),
            );

            if let Some(error) = runtime_invariant_failure(state, table) {
                *state = state_before_action;
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

pub(crate) fn runtime_invariant_failure(
    state: &EntityState,
    table: &TransitionTable,
) -> Option<String> {
    let Some(fields) = state.fields.as_object() else {
        return (!table.runtime_invariants.is_empty()).then(|| {
            "Action rejected because runtime safety state fields are not an object".to_string()
        });
    };
    table.runtime_invariants.iter().find_map(|invariant| {
        if invariant.enforcement_version
            != temper_spec::automaton::RUNTIME_INVARIANT_ENFORCEMENT_VERSION
        {
            return Some(format!(
                "Unsupported runtime safety contract version {} for invariant '{}'",
                invariant.enforcement_version, invariant.name
            ));
        }
        let active = invariant.when.is_empty() || invariant.when.contains(&state.status);
        (active
            && !temper_spec::automaton::evaluate_runtime_assert(
                &invariant.assertion,
                &state.status,
                &state.counters,
                &state.booleans,
                fields,
            ))
        .then(|| {
            format!(
                "Action rejected by runtime safety invariant '{}': {:?}",
                invariant.name, invariant.assertion
            )
        })
    })
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
) -> Result<AppliedEffects, String> {
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
                let item_count = state
                    .item_count
                    .checked_add(1)
                    .ok_or_else(|| "Counter 'items' overflowed its runtime range".to_string())?;
                let counter = state.counters.get("items").copied().unwrap_or_default();
                let counter = counter
                    .checked_add(1)
                    .ok_or_else(|| "Counter 'items' overflowed its runtime range".to_string())?;
                state.item_count = item_count;
                state.counters.insert("items".to_string(), counter);
            }
            Effect::DecrementItems => {
                state.item_count = state.item_count.saturating_sub(1);
                let c = state.counters.entry("items".to_string()).or_default();
                *c = c.saturating_sub(1);
            }
            Effect::IncrementCounter(var) => {
                let value = state.counters.get(var).copied().unwrap_or_default();
                let value = value
                    .checked_add(1)
                    .ok_or_else(|| format!("Counter '{var}' overflowed its runtime range"))?;
                // Keep legacy item_count in sync.
                if var == "items" {
                    state.item_count = state.item_count.checked_add(1).ok_or_else(|| {
                        "Counter 'items' overflowed its runtime range".to_string()
                    })?;
                }
                state.counters.insert(var.clone(), value);
            }
            Effect::IncrementCounterByParam { var, param } => {
                let delta = counter_delta_from_params(params, param);
                let value = state.counters.get(var).copied().unwrap_or_default();
                let value = value
                    .checked_add(delta)
                    .ok_or_else(|| format!("Counter '{var}' overflowed its runtime range"))?;
                if var == "items" {
                    state.item_count = state.item_count.checked_add(delta).ok_or_else(|| {
                        "Counter 'items' overflowed its runtime range".to_string()
                    })?;
                }
                state.counters.insert(var.clone(), value);
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

    Ok((
        custom_effects,
        scheduled_actions,
        spawn_requests,
        schedule_at_requests,
    ))
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

    #[test]
    fn counter_overflow_rejects_action_and_rolls_back_all_tentative_effects() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(213);
        let spec = r#"
[automaton]
name = "Meter"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "used"
type = "counter"
initial = "0"

[[action]]
name = "Consume"
from = ["Active"]
to = "Active"
effect = [{ type = "increment", var = "used" }]
"#;
        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state = build_initial_entity_state("Meter", "m-1", &table, &serde_json::json!({}))
            .expect("build initial meter state");
        state.counters.insert("used".into(), usize::MAX);
        let before = serde_json::to_value(&state).unwrap();

        let result = process_action(&mut state, &table, "Consume", &serde_json::json!({}));

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("overflowed its runtime range"))
        );
        assert_eq!(
            serde_json::to_value(&state).unwrap(),
            before,
            "overflow must not leave partial state"
        );
        assert!(result.event.is_none(), "rejected overflow is never durable");
    }

    #[test]
    fn invalid_declared_initial_values_fail_instead_of_falling_back_to_defaults() {
        let spec = r#"
[automaton]
name = "Meter"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "used"
type = "counter"
initial = "7"

[[state]]
name = "label"
type = "string"
initial = "ready"

[[state]]
name = "limit"
type = "counter"
initial = "10"

[[invariant]]
name = "WithinLimit"
when = ["Active"]
assert = "used <= limit"

[[invariant]]
name = "LabelRequired"
when = ["Active"]
assert = "label != ''"
"#;
        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);

        let counter_error = build_initial_entity_state(
            "Meter",
            "m-invalid-counter",
            &table,
            &serde_json::json!({"used": "not-a-counter"}),
        )
        .expect_err("invalid counter must not retain the declared default");
        assert!(counter_error.contains("must be a usize"));

        let forged_blob_error = build_initial_entity_state(
            "Meter",
            "m-invalid-string",
            &table,
            &serde_json::json!({
                "label": {
                    "__temper_blob_ref": "fake",
                    "__temper_blob_encoding": "json"
                }
            }),
        )
        .expect_err("caller-supplied blob envelope must not satisfy a declared string");
        assert!(forged_blob_error.contains("must be a string"));
    }

    #[test]
    fn oversized_nonempty_string_blob_ref_preserves_runtime_invariant_semantics() {
        let _guard = temper_runtime::scheduler::install_deterministic_context(213);
        let spec = r#"
[automaton]
name = "Document"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "body"
type = "string"
initial = "ready"
overflow_inline_max_bytes = 8

[[invariant]]
name = "BodyRequired"
when = ["Active"]
assert = "body != ''"

[[action]]
name = "Update"
from = ["Active"]
to = "Active"
"#;
        let table = temper_jit::table::TransitionTable::from_ioa_source(spec);
        let mut state =
            build_initial_entity_state("Document", "d-1", &table, &serde_json::json!({}))
                .expect("build initial document state");

        let forged = process_action_with_xref_and_field_mode(
            &mut state,
            &table,
            "Update",
            &serde_json::json!({
                "body": {
                    "__temper_blob_ref": "fake",
                    "__temper_blob_encoding": "json"
                }
            }),
            &std::collections::BTreeMap::new(),
            FieldSyncMode::BlobRefs {
                default_inline_max: DEFAULT_FIELD_INLINE_MAX,
            },
        );
        assert!(
            !forged.success,
            "caller-supplied blob envelopes are not strings"
        );
        assert!(
            forged
                .error
                .as_deref()
                .is_some_and(|error| error.contains("must be a string"))
        );

        let result = process_action_with_xref_and_field_mode(
            &mut state,
            &table,
            "Update",
            &serde_json::json!({"body": "a durable nonempty body"}),
            &std::collections::BTreeMap::new(),
            FieldSyncMode::BlobRefs {
                default_inline_max: DEFAULT_FIELD_INLINE_MAX,
            },
        );

        assert!(
            result.success,
            "blob projection must preserve logical non-emptiness"
        );
        assert_eq!(result.overflow_blobs.len(), 1);
        assert!(
            state
                .fields
                .get("body")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|object| object.contains_key("__temper_blob_ref"))
        );
        assert!(runtime_invariant_failure(&state, &table).is_none());
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
            apply_effects(&mut state, &effects, &serde_json::json!({})).unwrap();

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
        )
        .unwrap();

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
}
