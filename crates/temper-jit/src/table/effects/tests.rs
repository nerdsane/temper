use super::*;

#[derive(Debug, Default, PartialEq)]
struct TestState {
    status: String,
    item_count: usize,
    counters: BTreeMap<String, usize>,
    booleans: BTreeMap<String, bool>,
    lists: BTreeMap<String, Vec<String>>,
    fields: serde_json::Value,
}

impl EffectState for TestState {
    fn status(&self) -> &str {
        &self.status
    }

    fn status_mut(&mut self) -> &mut String {
        &mut self.status
    }

    fn legacy_item_count(&self) -> Option<usize> {
        Some(self.item_count)
    }

    fn legacy_item_count_mut(&mut self) -> Option<&mut usize> {
        Some(&mut self.item_count)
    }

    fn counters(&self) -> &BTreeMap<String, usize> {
        &self.counters
    }

    fn counters_mut(&mut self) -> &mut BTreeMap<String, usize> {
        &mut self.counters
    }

    fn booleans(&self) -> &BTreeMap<String, bool> {
        &self.booleans
    }

    fn booleans_mut(&mut self) -> &mut BTreeMap<String, bool> {
        &mut self.booleans
    }

    fn lists(&self) -> &BTreeMap<String, Vec<String>> {
        &self.lists
    }

    fn lists_mut(&mut self) -> &mut BTreeMap<String, Vec<String>> {
        &mut self.lists
    }

    fn fields(&self) -> &serde_json::Value {
        &self.fields
    }

    fn fields_mut(&mut self) -> &mut serde_json::Value {
        &mut self.fields
    }
}

#[test]
fn exhaustive_effect_execution_is_deterministic() {
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
        "add": "4",
        "subtract": 2,
        "limit": 9,
        "entries": "second",
        "entries_index": 0,
        "child_id": "child-1",
    });
    let mut state = TestState {
        status: "Idle".into(),
        lists: BTreeMap::from([("entries".into(), vec!["first".into()])]),
        fields: serde_json::json!({"owner": "owner-1"}),
        ..Default::default()
    };

    let execution = apply_effects(&mut state, &effects, &params);

    assert_eq!(state.status, "Ready");
    assert_eq!(state.item_count, 0);
    assert_eq!(state.counters["items"], 0);
    assert_eq!(state.counters["count"], 2);
    assert_eq!(state.counters["limit"], 9);
    assert!(state.booleans["enabled"]);
    assert_eq!(state.lists["entries"], ["second"]);
    assert_eq!(state.fields["spawned_id"], "child-1");
    assert_eq!(execution.emitted_events, ["Changed"]);
    assert_eq!(execution.custom_effects, ["Notify"]);
    assert_eq!(
        execution.scheduled_actions,
        [ScheduledAction {
            action: "Wake".into(),
            delay_seconds: 5,
        }]
    );
    assert_eq!(
        execution.schedule_at_requests,
        [ScheduleAtRequest {
            action: "Expire".into(),
            field: "expires_at".into(),
        }]
    );
    assert_eq!(execution.spawn_requests[0].entity_id, "child-1");
    assert_eq!(
        execution.spawn_requests[0].copied_field_values["owner"],
        "owner-1"
    );
}

#[test]
fn generated_spawn_ids_repeat_with_the_same_seed() {
    let effects = [Effect::SpawnEntity {
        entity_type: "Child".into(),
        entity_id_source: "{uuid}".into(),
        initial_action: Some("Start".into()),
        store_id_in: Some("spawned_id".into()),
        copy_fields: Some(vec!["owner".into()]),
    }];
    let params = serde_json::json!({});

    let run = || {
        let (_guard, _, _) = temper_runtime::scheduler::install_deterministic_context(0x179);
        let mut state = TestState {
            status: "Idle".into(),
            fields: serde_json::json!({"owner": "owner-1"}),
            ..Default::default()
        };
        let execution = apply_effects(&mut state, &effects, &params);
        (state, execution)
    };

    let first = run();
    let second = run();

    assert_eq!(first, second);
    assert_eq!(
        first.0.fields["spawned_id"],
        first.1.spawn_requests[0].entity_id
    );
}
