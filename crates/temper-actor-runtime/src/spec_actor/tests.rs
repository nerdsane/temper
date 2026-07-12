use super::*;

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

const DURABLE_LIST_SPEC: &str = r#"
[automaton]
name = "DurableListActor"
states = ["Collecting", "Ready"]
initial = "Collecting"

[[state]]
name = "entries"
type = "list"
initial = "[]"

[[action]]
name = "Append"
kind = "input"
from = ["Collecting"]
to = "Collecting"
effect = [{ type = "list_append", var = "entries" }]

[[action]]
name = "Finish"
kind = "input"
from = ["Collecting"]
to = "Ready"
guard = [{ type = "list_length_min", var = "entries", min = 1 }]
"#;

fn spec_message(id: i64, to: &ActorHandle, action: &str, params: serde_json::Value) -> Message {
    Message {
        id,
        from: None,
        to: to.clone(),
        message_type: "SpecMessage".to_string(),
        payload: prost::Message::encode_to_vec(&SpecMessage::with_params(action, params)),
        correlation_id: None,
        created_at: chrono::Utc::now(),
    }
}

#[test]
fn test_spec_driven_actor_initial_state() {
    let actor = SpecDrivenActor::from_ioa(SIMPLE_SPEC, HashMap::new()).unwrap();
    let state_bytes = actor.initial_state();
    let state: SpecActorState = serde_json::from_slice(&state_bytes).unwrap();
    assert_eq!(state.status, "Idle");
    assert_eq!(state.counters.get("rounds"), Some(&0usize));
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

#[tokio::test]
async fn list_effect_survives_serialization_and_enables_later_guard() {
    let handle = ActorHandle::new("default/entity-1", "DurableListActor");
    let context = ActorContext::new(handle.clone(), None, None);
    let actor = SpecDrivenActor::from_ioa(DURABLE_LIST_SPEC, HashMap::new())
        .expect("durability regression spec must parse");
    let mut persisted_state = actor.initial_state();

    actor
        .handle(
            &context,
            &mut persisted_state,
            &spec_message(
                1,
                &handle,
                "Append",
                serde_json::json!({"entries": "durable-value"}),
            ),
        )
        .await
        .expect("append transition must complete");

    let reactivated_actor = SpecDrivenActor::from_ioa(DURABLE_LIST_SPEC, HashMap::new())
        .expect("reactivated actor must rebuild from the same spec");
    reactivated_actor
        .handle(
            &context,
            &mut persisted_state,
            &spec_message(2, &handle, "Finish", serde_json::json!({})),
        )
        .await
        .expect("guarded transition must be evaluated");

    let state: SpecActorState = serde_json::from_slice(&persisted_state)
        .expect("reactivated actor state must remain serializable");
    assert_eq!(state.lists["entries"], ["durable-value"]);
    assert_eq!(
        state.status, "Ready",
        "the persisted append must satisfy the later list-length guard"
    );
}
