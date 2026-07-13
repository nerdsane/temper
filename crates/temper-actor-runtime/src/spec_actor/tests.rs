use super::*;

#[path = "tests/reactions.rs"]
mod reactions;

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

const DURABLE_COMMAND_SPEC: &str = r#"
[automaton]
name = "CommandActor"
states = ["Idle", "Ready"]
initial = "Idle"

[[action]]
name = "Run"
kind = "input"
from = ["Idle"]
to = "Ready"
effect = [
  { type = "schedule", action = "Wake", delay_seconds = 5 },
  { type = "schedule_at", action = "Expire", field = "expires_at" },
  { type = "spawn", entity_type = "Child", entity_id_source = "child_id", initial_action = "Start", store_id_in = "spawned_id", copy_fields = "owner" },
]
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
    let actor = SpecDrivenActor::from_ioa(SIMPLE_SPEC, ReactionRegistry::new()).unwrap();
    let state_bytes = actor.initial_state();
    let state: SpecActorState = serde_json::from_slice(&state_bytes).unwrap();
    assert_eq!(state.status, "Idle");
    assert_eq!(state.counters.get("rounds"), Some(&0usize));
}

#[test]
fn reaction_registry_preserves_declared_rule() {
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
            params: serde_json::Value::Null,
            params_from: std::collections::BTreeMap::new(),
        },
        resolve_target: temper_runtime::reaction::TargetResolver::SameId,
    }];

    let registry = ReactionRegistry::from(rules);
    let matches = registry.lookup("Agent", "PrepareContext", "Ready");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].then.entity_type, "ContextManager");
    assert_eq!(matches[0].then.action, "PrepareContext");
}

#[tokio::test]
async fn timer_and_spawn_effects_are_buffered_for_atomic_persistence() {
    let actor = SpecDrivenActor::from_ioa(DURABLE_COMMAND_SPEC, ReactionRegistry::new())
        .expect("durable command spec must parse");
    let handle = ActorHandle::new("default/parent-1", "CommandActor");
    let context = ActorContext::new(handle.clone(), None, None);
    let mut state = actor.initial_state();

    actor
        .handle(
            &context,
            &mut state,
            &spec_message(
                1,
                &handle,
                "Run",
                serde_json::json!({
                    "expires_at": "2030-01-01T00:00:00Z",
                    "child_id": "child-1",
                    "owner": "owner-1",
                }),
            ),
        )
        .await
        .expect("runtime commands must be buffered");

    let state: SpecActorState =
        serde_json::from_slice(&state).expect("command actor state must serialize");
    assert_eq!(state.status, "Ready");
    assert_eq!(state.fields["spawned_id"], "child-1");

    let tells = context.take_pending_tells().await;
    assert_eq!(tells.len(), 2);
    assert!(tells.iter().all(|tell| tell.deliver_at.is_some()));
    let spawns = context.take_pending_spawns().await;
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].handle.namespace, "default/child-1");
    assert_eq!(spawns[0].fields["owner"], "owner-1");
    assert!(spawns[0].initial_message.is_some());
}

#[tokio::test]
async fn list_effect_survives_serialization_and_enables_later_guard() {
    let handle = ActorHandle::new("default/entity-1", "DurableListActor");
    let context = ActorContext::new(handle.clone(), None, None);
    let actor = SpecDrivenActor::from_ioa(DURABLE_LIST_SPEC, ReactionRegistry::new())
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

    let reactivated_actor = SpecDrivenActor::from_ioa(DURABLE_LIST_SPEC, ReactionRegistry::new())
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
