//! Tests for process / apply / field projection.

use super::*;
use crate::entity_actor::types::EntityState;
use temper_jit::table::{Effect, TransitionTable};
use temper_runtime::scheduler::sim_now;

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

#[test]
fn action_processing_strips_runtime_owned_fields_from_state_and_event() {
    let _guard = temper_runtime::scheduler::install_deterministic_context(42);
    let table = temper_jit::table::TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Document"
states = ["Initial", "Approved"]
initial = "Initial"

[[action]]
name = "Approve"
kind = "input"
from = ["Initial"]
to = "Approved"
params = ["Title"]
"#,
    );
    let mut state = make_state("Document", "doc-1");
    state.fields = serde_json::json!({
        "Id": "forged-before",
        "Status": "forged-before",
        "ctx_owner_status": "Privileged"
    });
    let result = process_action(
        &mut state,
        &table,
        "Approve",
        &serde_json::json!({
            "Id": "forged-after",
            "id": "forged-after",
            "Status": "Rejected",
            "status": "Rejected",
            "has_spec": false,
            "HasSpec": false,
            "ctx_owner_status": "Privileged",
            "Title": "Trusted title"
        }),
    );

    assert!(result.success);
    assert_eq!(state.fields["id"], "doc-1");
    assert_eq!(state.fields["Id"], "doc-1");
    assert_eq!(state.fields["status"], "Approved");
    assert_eq!(state.fields["Status"], "Approved");
    assert_eq!(state.fields["Title"], "Trusted title");
    for reserved in ["has_spec", "HasSpec", "ctx_owner_status"] {
        assert!(state.fields.get(reserved).is_none(), "persisted {reserved}");
    }
    let event = result.event.expect("successful action event");
    assert_eq!(event.params, serde_json::json!({"Title": "Trusted title"}));
}
