use std::collections::HashMap;

use super::*;
use crate::actor::ActorHandle;

const SIMPLE_SPEC: &str = r#"
[automaton]
name = "TestActor"
states = ["Idle", "Running"]
initial = "Idle"

[[state]]
name = "rounds"
type = "counter"
initial = "0"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Running"
effect = [{ type = "increment", var = "rounds" }]

[[action]]
name = "Stop"
kind = "input"
from = ["Running"]
to = "Idle"
"#;

#[test]
fn test_spec_driven_actor_initial_state() {
    let actor = SpecDrivenActor::from_ioa(SIMPLE_SPEC, HashMap::new()).unwrap();
    let state_bytes = actor.initial_state();
    let state: SpecActorState = serde_json::from_slice(&state_bytes).unwrap();
    assert_eq!(state.status, "Idle");
    assert_eq!(state.counters.get("rounds"), Some(&0usize));
}

/// Spec exercising every param-driven state effect plus a guard that
/// reads the list those effects maintain.
const PARAM_EFFECTS_SPEC: &str = r#"
[automaton]
name = "Inventory"
states = ["Active", "Locked"]
initial = "Active"

[[state]]
name = "tags"
type = "list"

[[state]]
name = "total"
type = "counter"
initial = "0"

[[state]]
name = "progress"
type = "counter"
initial = "0"

[[action]]
name = "AddTag"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{ type = "list_append", var = "tags" }]

[[action]]
name = "RemoveTag"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{ type = "list_remove_at", var = "tags" }]

[[action]]
name = "RecordBatch"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{ type = "increment", var = "total", amount = "qty" }]

[[action]]
name = "ConsumeBatch"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{ type = "decrement", var = "total", amount = "qty" }]

[[action]]
name = "SetProgress"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{ type = "set_counter_from_param", var = "progress", param = "value" }]

[[action]]
name = "Lock"
kind = "input"
from = ["Active"]
to = "Locked"
guard = "list_length_min tags 1"
"#;

/// Send one action to the actor and return the updated state.
async fn drive(
    actor: &SpecDrivenActor,
    state: &mut Vec<u8>,
    action: &str,
    params: serde_json::Value,
) -> SpecActorState {
    use prost::Message as _;

    let handle = ActorHandle::new("test-ns", actor.actor_type());
    let ctx = ActorContext::new(handle.clone(), None, None);
    let spec_msg = SpecMessage::with_params(action, params);
    let message = Message {
        id: 1,
        from: None,
        to: handle,
        message_type: "SpecMessage".to_string(),
        payload: spec_msg.encode_to_vec(),
        correlation_id: None,
        created_at: chrono::Utc::now(),
    };
    actor
        .handle(&ctx, state, &message)
        .await
        .expect("handle should succeed");
    serde_json::from_slice(state).expect("state should deserialize")
}

#[tokio::test]
async fn param_driven_effects_mutate_state() {
    let actor =
        SpecDrivenActor::from_ioa(PARAM_EFFECTS_SPEC, HashMap::new()).expect("spec should build");
    let mut state = actor.initial_state();

    let s = drive(
        &actor,
        &mut state,
        "AddTag",
        serde_json::json!({"tags": "urgent"}),
    )
    .await;
    assert_eq!(
        s.lists.get("tags").map(Vec::as_slice),
        Some(["urgent".to_string()].as_slice()),
        "list_append must append the param value"
    );

    let s = drive(
        &actor,
        &mut state,
        "RecordBatch",
        serde_json::json!({"qty": 5}),
    )
    .await;
    assert_eq!(
        s.counters.get("total"),
        Some(&5),
        "increment-by-param must add the param delta"
    );

    let s = drive(
        &actor,
        &mut state,
        "ConsumeBatch",
        serde_json::json!({"qty": 2}),
    )
    .await;
    assert_eq!(
        s.counters.get("total"),
        Some(&3),
        "decrement-by-param must subtract the param delta"
    );

    let s = drive(
        &actor,
        &mut state,
        "SetProgress",
        serde_json::json!({"value": 42}),
    )
    .await;
    assert_eq!(
        s.counters.get("progress"),
        Some(&42),
        "set_counter_from_param must set the counter"
    );

    let s = drive(
        &actor,
        &mut state,
        "RemoveTag",
        serde_json::json!({"tags_index": 0}),
    )
    .await;
    assert_eq!(
        s.lists.get("tags").map(Vec::len),
        Some(0),
        "list_remove_at must remove the indexed element"
    );
}

/// The mis-gating scenario from ARN-179: a guard reading a list that a
/// prior transition appended to must see the appended value.
#[tokio::test]
async fn list_guard_sees_appended_value() {
    let actor =
        SpecDrivenActor::from_ioa(PARAM_EFFECTS_SPEC, HashMap::new()).expect("spec should build");
    let mut state = actor.initial_state();

    drive(
        &actor,
        &mut state,
        "AddTag",
        serde_json::json!({"tags": "urgent"}),
    )
    .await;
    let s = drive(&actor, &mut state, "Lock", serde_json::json!({})).await;
    assert_eq!(
        s.status, "Locked",
        "list_length_min guard must pass after list_append appended a value"
    );
}

#[test]
fn unexecutable_effects_rejected_at_construction() {
    for (label, effect) in [
        (
            "schedule",
            r#"{ type = "schedule", action = "Expire", delay_seconds = 60 }"#,
        ),
        (
            "schedule_at",
            r#"{ type = "schedule_at", action = "Expire", field = "expires_at" }"#,
        ),
        (
            "spawn",
            r#"{ type = "spawn", entity_type = "Child", entity_id_source = "{uuid}" }"#,
        ),
    ] {
        let spec = format!(
            r#"
[automaton]
name = "Scheduler"
states = ["Active"]
initial = "Active"

[[action]]
name = "Arm"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{effect}]
"#
        );
        let result = SpecDrivenActor::from_ioa(&spec, HashMap::new());
        let error = match result {
            Ok(_) => {
                panic!("{label} effect must be rejected at construction, not silently dropped")
            }
            Err(error) => error,
        };
        assert!(
            error.contains(label),
            "rejection message must name the unsupported {label} effect, got: {error}"
        );
    }
}

#[test]
fn unrouted_trigger_rejected_at_construction() {
    const TRIGGER_SPEC: &str = r#"
[automaton]
name = "Notifier"
states = ["Active"]
initial = "Active"

[[action]]
name = "Notify"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{ type = "trigger", name = "SendNotification" }]
"#;

    let unrouted = SpecDrivenActor::from_ioa(TRIGGER_SPEC, HashMap::new());
    assert!(
        unrouted.is_err(),
        "a trigger effect with no reaction routing must be rejected at construction"
    );

    let mut routing = HashMap::new();
    routing.insert(
        "SendNotification".to_string(),
        ("NotifierIntegration".to_string(), "send".to_string()),
    );
    let routed = SpecDrivenActor::from_ioa(TRIGGER_SPEC, routing);
    assert!(
        routed.is_ok(),
        "a routed trigger effect must be accepted: {:?}",
        routed.err()
    );
}

#[test]
fn test_routing_map_builder() {
    let rules = vec![ReactionRule {
        name: "a".into(),
        when: temper_runtime::reaction::ReactionTrigger {
            entity_type: "Agent".into(),
            action: Some("PrepareContext".into()),
            to_state: None,
        },
        then: temper_runtime::reaction::ReactionTarget {
            entity_type: "ContextManager".into(),
            action: "PrepareContext".into(),
        },
        resolve_target: temper_runtime::reaction::TargetResolver::SameId,
    }];

    let maps = build_routing_maps(&rules);
    assert_eq!(maps["Agent"]["PrepareContext"].0, "ContextManager");
    assert_eq!(maps["Agent"]["PrepareContext"].1, "PrepareContext");
}
