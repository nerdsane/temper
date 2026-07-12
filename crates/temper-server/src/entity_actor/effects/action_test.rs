use super::super::types::EntityState;
use super::*;
use temper_jit::table::Effect;

#[test]
fn server_and_postgres_adapters_produce_identical_effect_results() {
    use temper_actor_runtime::SpecActorState;
    use temper_jit::table::{Effect, apply_effects as apply_canonical_effects};

    let effects = vec![
        Effect::SetState("Ready".into()),
        Effect::IncrementItems,
        Effect::DecrementItems,
        Effect::IncrementCounter("count".into()),
        Effect::IncrementCounterByParam {
            var: "count".into(),
            param: "add".into(),
        },
        Effect::DecrementCounter("count".into()),
        Effect::DecrementCounterByParam {
            var: "count".into(),
            param: "subtract".into(),
        },
        Effect::SetCounterFromParam {
            var: "limit".into(),
            param: "limit".into(),
        },
        Effect::SetBool {
            var: "enabled".into(),
            value: true,
        },
        Effect::EmitEvent("Changed".into()),
        Effect::ListAppend("entries".into()),
        Effect::ListRemoveAt("entries".into()),
        Effect::Custom("Notify".into()),
        Effect::ScheduleAction {
            action: "Wake".into(),
            delay_seconds: 5,
        },
        Effect::ScheduleAtAction {
            action: "Expire".into(),
            field: "expires_at".into(),
        },
        Effect::SpawnEntity {
            entity_type: "Child".into(),
            entity_id_source: "child_id".into(),
            initial_action: Some("Start".into()),
            store_id_in: Some("spawned_id".into()),
            copy_fields: Some(vec!["owner".into()]),
        },
    ];
    let params = serde_json::json!({
        "add": 4,
        "subtract": 2,
        "limit": 9,
        "entries": "second",
        "entries_index": 0,
        "child_id": "child-1",
    });
    let fields = serde_json::json!({"owner": "owner-1"});
    let lists =
        std::collections::BTreeMap::from([("entries".to_string(), vec!["first".to_string()])]);
    let mut server_state = EntityState {
        entity_type: "Parent".into(),
        entity_id: "parent-1".into(),
        status: "Idle".into(),
        item_count: 0,
        counters: Default::default(),
        booleans: Default::default(),
        lists: lists.clone(),
        fields: fields.clone(),
        events: Default::default(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: Default::default(),
    };
    let mut postgres_state = SpecActorState {
        status: "Idle".into(),
        lists,
        fields,
        ..Default::default()
    };

    let server_commands = apply_canonical_effects(&mut server_state, &effects, &params);
    let postgres_commands = apply_canonical_effects(&mut postgres_state, &effects, &params);

    assert_eq!(server_state.status, postgres_state.status);
    assert_eq!(server_state.counters, postgres_state.counters);
    assert_eq!(server_state.booleans, postgres_state.booleans);
    assert_eq!(server_state.lists, postgres_state.lists);
    assert_eq!(server_state.fields, postgres_state.fields);
    assert_eq!(server_commands, postgres_commands);
}

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
