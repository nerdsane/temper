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
use temper_jit::table::{Effect, EvalContext, TransitionTable};
use temper_runtime::scheduler::{sim_now, sim_uuid};

use super::types::{EntityEvent, EntityState};

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
}

/// Maximum cross-entity lookups per transition (TigerStyle budget).
pub const MAX_CROSS_ENTITY_LOOKUPS: usize = 4;
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
    process_action_with_xref(
        state,
        table,
        action,
        params,
        &std::collections::BTreeMap::new(),
    )
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
    let ctx = build_eval_context_with_xref(state, cross_entity_booleans);
    let result = table.evaluate_ctx(&state.status, &ctx, action);

    match result {
        Some(transition_result) if transition_result.success => {
            let from_status = state.status.clone();
            let to_status = transition_result.new_state.clone();

            let (custom_effects, scheduled_actions, spawn_requests) =
                apply_effects(state, &transition_result.effects, params);
            apply_new_state_fallback(state, &from_status, &to_status);
            sync_fields(state, params);

            let event = EntityEvent {
                action: action.to_string(),
                from_status,
                to_status: state.status.clone(),
                timestamp: sim_now(),
                params: params.clone(),
            };

            ProcessResult {
                success: true,
                event: Some(event),
                custom_effects,
                scheduled_actions,
                spawn_requests,
                error: None,
            }
        }
        Some(_) => ProcessResult {
            success: false,
            event: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            error: Some(format!(
                "Action '{}' not valid from state '{}'",
                action, state.status
            )),
        },
        None => ProcessResult {
            success: false,
            event: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            error: Some(format!("Unknown action: {}", action)),
        },
    }
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
/// A tuple of (custom effect names, scheduled actions).
pub fn apply_effects(
    state: &mut EntityState,
    effects: &[Effect],
    params: &serde_json::Value,
) -> (Vec<String>, Vec<ScheduledAction>, Vec<SpawnRequest>) {
    let mut custom_effects = Vec::new();
    let mut scheduled_actions = Vec::new();
    let mut spawn_requests = Vec::new();

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

                spawn_requests.push(SpawnRequest {
                    entity_type: entity_type.clone(),
                    entity_id: child_id.clone(),
                    initial_action: initial_action.clone(),
                    store_id_in: store_id_in.clone(),
                });

                tracing::info!(
                    entity_type = %state.entity_type,
                    entity_id = %state.entity_id,
                    child_type = %entity_type,
                    child_id = %child_id,
                    "spawn entity request"
                );
            }
        }
    }

    (custom_effects, scheduled_actions, spawn_requests)
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
            obj.insert(k.clone(), serde_json::Value::Array(arr));
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
            events: vec![],
            sequence_nr: 0,
        };

        let result = process_action(&mut state, &table, "Activate", &serde_json::json!({}));

        assert!(result.success, "action should succeed");
        assert_eq!(state.status, "Active");
        assert_eq!(result.scheduled_actions.len(), 1);
        assert_eq!(result.scheduled_actions[0].action, "Refresh");
        assert_eq!(result.scheduled_actions[0].delay_seconds, 2700);
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
            events: vec![],
            sequence_nr: 0,
        };

        let (custom, scheduled, _spawns) =
            apply_effects(&mut state, &effects, &serde_json::json!({}));

        assert!(custom.is_empty());
        assert_eq!(scheduled.len(), 1);
        assert_eq!(scheduled[0].action, "Refresh");
        assert_eq!(scheduled[0].delay_seconds, 3600);
        assert_eq!(state.status, "Active");
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
            events: vec![],
            sequence_nr: 0,
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
            events: vec![],
            sequence_nr: 0,
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
}
