use super::*;

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
        state_timeout_clock_reset_at: None,
        state_timeout_clock_reset_version: None,
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
