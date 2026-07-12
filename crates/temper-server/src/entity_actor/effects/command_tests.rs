use super::super::types::EntityState;
use super::*;
use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::sim_now;

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
