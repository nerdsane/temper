use super::*;

#[test]
fn test_schedule_effect_maps_to_schedule_action() {
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

    let table = TransitionTable::from_ioa_source(spec);
    let rule = table.rules.iter().find(|r| r.name == "Activate").unwrap();

    let has_schedule = rule.effects.iter().any(|e| {
        matches!(
            e,
            Effect::ScheduleAction { action, delay_seconds }
                if action == "Refresh" && *delay_seconds == 2700
        )
    });
    assert!(
        has_schedule,
        "expected ScheduleAction effect, got: {:?}",
        rule.effects
    );
}

#[test]
fn composite_metadata_is_registered_on_transition_table() {
    let spec = r#"
[automaton]
name = "Repository"
states = ["Active"]
initial = "Active"

[[action]]
name = "IngestPack"
kind = "Composite"
from = ["Active"]
to = "Active"
record_parent_event = false

[[action.cedar_gate]]
principal = "request.principal"
resource = "this"
action = "Repository::IngestPack"

[[action.sub_writes]]
target_entity = "Blob"
action = "Create"
generated_from = "pack_bytes"
"#;

    let table = TransitionTable::from_ioa_source(spec);
    let metadata = table.composite_actions.get("IngestPack").unwrap();

    assert_eq!(
        metadata
            .cedar_gate
            .as_ref()
            .map(|gate| gate.action.as_str()),
        Some("Repository::IngestPack")
    );
    assert!(!metadata.record_parent_event);
    assert_eq!(metadata.sub_writes.len(), 1);
    assert_eq!(metadata.sub_writes[0].target_entity, "Blob");
}
