use super::*;
use crate::actor::{ActorContext, ActorHandle, Message};

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

// ─── ARN-179 durability regressions ───────────────────────────────────

const LIST_SPEC: &str = r#"
[automaton]
name = "ListActor"
states = ["Idle", "Active"]
initial = "Idle"

[[state]]
name = "tags"
type = "list"
initial = "[]"

[[action]]
name = "AddTag"
kind = "input"
from = ["Idle", "Active"]
to = "Active"
effect = [{ type = "list_append", var = "tags" }]
"#;

const COUNTER_PARAM_SPEC: &str = r#"
[automaton]
name = "CounterActor"
states = ["Idle", "Active"]
initial = "Idle"

[[state]]
name = "score"
type = "counter"
initial = "0"

[[action]]
name = "SetScore"
kind = "input"
from = ["Idle", "Active"]
to = "Active"
effect = [{ type = "set_counter_from_param", var = "score", param = "score" }]
"#;

const SCHEDULE_SPEC: &str = r#"
[automaton]
name = "TimerActor"
states = ["Idle", "Waiting"]
initial = "Idle"

[[action]]
name = "Arm"
kind = "input"
from = ["Idle"]
to = "Waiting"
effect = [{ type = "schedule", action = "Fire", delay_seconds = 5 }]

[[action]]
name = "Fire"
kind = "input"
from = ["Waiting"]
to = "Idle"
"#;

const SCHEDULE_AT_SPEC: &str = r#"
[automaton]
name = "TimerAtActor"
states = ["Idle", "Waiting"]
initial = "Idle"

[[action]]
name = "Arm"
kind = "input"
from = ["Idle"]
to = "Waiting"
effect = [{ type = "schedule_at", action = "Fire", field = "due_at" }]

[[action]]
name = "Fire"
kind = "input"
from = ["Waiting"]
to = "Idle"
"#;

const SPAWN_SPEC: &str = r#"
[automaton]
name = "ParentActor"
states = ["Idle", "Spawned"]
initial = "Idle"

[[action]]
name = "CreateChild"
kind = "input"
from = ["Idle"]
to = "Spawned"
effect = [{ type = "spawn", entity_type = "Child", entity_id_source = "{uuid}" }]
"#;

const COUNTER_DELTA_SPEC: &str = r#"
[automaton]
name = "DeltaActor"
states = ["Idle", "Active"]
initial = "Idle"

[[state]]
name = "score"
type = "counter"
initial = "10"

[[action]]
name = "Bump"
kind = "input"
from = ["Idle", "Active"]
to = "Active"
effect = [
  { type = "increment", var = "score", amount = "delta" },
  { type = "decrement", var = "score", amount = "penalty" },
]
"#;

const LIST_REMOVE_SPEC: &str = r#"
[automaton]
name = "ListRemoveActor"
states = ["Idle", "Active"]
initial = "Idle"

[[state]]
name = "tags"
type = "list"
initial = "[]"

[[action]]
name = "AddTag"
kind = "input"
from = ["Idle", "Active"]
to = "Active"
effect = [{ type = "list_append", var = "tags" }]

[[action]]
name = "DropTag"
kind = "input"
from = ["Active"]
to = "Active"
effect = [{ type = "list_remove_at", var = "tags" }]
"#;

fn test_message(action: &str, params: serde_json::Value) -> Message {
    use prost::Message as _;
    let payload = SpecMessage::with_params(action, params);
    Message {
        id: 1,
        from: None,
        to: ActorHandle::new("test-ns", "TestActor"),
        message_type: "SpecMessage".into(),
        payload: payload.encode_to_vec(),
        correlation_id: None,
        created_at: chrono::Utc::now(),
    }
}

fn test_ctx(actor_type: &str) -> ActorContext {
    ActorContext::new(ActorHandle::new("test-ns", actor_type), None, None)
}

/// RED: ListAppend was silently dropped; durable list state stayed empty.
#[tokio::test]
async fn list_append_effect_persists_into_actor_state() {
    let actor = SpecDrivenActor::from_ioa(LIST_SPEC, HashMap::new()).expect("parse");
    let ctx = test_ctx("ListActor");
    let mut state = actor.initial_state();
    let msg = test_message("AddTag", serde_json::json!({"tags": "alpha"}));
    actor.handle(&ctx, &mut state, &msg).await.expect("handle");
    let s: SpecActorState = serde_json::from_slice(&state).expect("deser");
    let tags = s.lists.get("tags").cloned().unwrap_or_default();
    assert_eq!(
        tags,
        vec!["alpha".to_string()],
        "ListAppend must append the param value into durable list state (ARN-179)"
    );
    assert_eq!(s.status, "Active");
}

/// RED: SetCounterFromParam was silently dropped; counter stayed at 0.
#[tokio::test]
async fn set_counter_from_param_persists_into_actor_state() {
    let actor = SpecDrivenActor::from_ioa(COUNTER_PARAM_SPEC, HashMap::new()).expect("parse");
    let ctx = test_ctx("CounterActor");
    let mut state = actor.initial_state();
    let msg = test_message("SetScore", serde_json::json!({"score": 42}));
    actor.handle(&ctx, &mut state, &msg).await.expect("handle");
    let s: SpecActorState = serde_json::from_slice(&state).expect("deser");
    assert_eq!(
        s.counters.get("score"),
        Some(&42usize),
        "SetCounterFromParam must write the param into durable counter state (ARN-179)"
    );
}

/// Schedule effects must not load silently — reject at construction.
#[test]
fn schedule_effect_rejected_at_construction() {
    match SpecDrivenActor::from_ioa(SCHEDULE_SPEC, HashMap::new()) {
        Ok(_) => panic!("schedule effect must fail closed at construction"),
        Err(err) => assert!(
            err.contains("schedule") || err.contains("unsupported"),
            "error must name the unsupported schedule effect, got: {err}"
        ),
    }
}

#[test]
fn schedule_at_effect_rejected_at_construction() {
    match SpecDrivenActor::from_ioa(SCHEDULE_AT_SPEC, HashMap::new()) {
        Ok(_) => panic!("schedule_at effect must fail closed at construction"),
        Err(err) => assert!(
            err.contains("schedule_at") || err.contains("unsupported"),
            "error must name the unsupported schedule_at effect, got: {err}"
        ),
    }
}

#[test]
fn spawn_effect_rejected_at_construction() {
    match SpecDrivenActor::from_ioa(SPAWN_SPEC, HashMap::new()) {
        Ok(_) => panic!("spawn effect must fail closed at construction"),
        Err(err) => assert!(
            err.contains("spawn") || err.contains("unsupported"),
            "error must name the unsupported spawn effect, got: {err}"
        ),
    }
}

#[tokio::test]
async fn counter_by_param_deltas_apply() {
    let actor = SpecDrivenActor::from_ioa(COUNTER_DELTA_SPEC, HashMap::new()).expect("parse");
    let ctx = test_ctx("DeltaActor");
    let mut state = actor.initial_state();
    // start 10; +5; -3 => 12
    let msg = test_message("Bump", serde_json::json!({"delta": 5, "penalty": 3}));
    actor.handle(&ctx, &mut state, &msg).await.expect("handle");
    let s: SpecActorState = serde_json::from_slice(&state).expect("deser");
    assert_eq!(s.counters.get("score"), Some(&12usize));
}

#[tokio::test]
async fn list_remove_at_effect_removes_index() {
    let actor = SpecDrivenActor::from_ioa(LIST_REMOVE_SPEC, HashMap::new()).expect("parse");
    let ctx = test_ctx("ListRemoveActor");
    let mut state = actor.initial_state();
    actor
        .handle(
            &ctx,
            &mut state,
            &test_message("AddTag", serde_json::json!({"tags": "a"})),
        )
        .await
        .expect("add a");
    actor
        .handle(
            &ctx,
            &mut state,
            &test_message("AddTag", serde_json::json!({"tags": "b"})),
        )
        .await
        .expect("add b");
    actor
        .handle(
            &ctx,
            &mut state,
            &test_message("DropTag", serde_json::json!({"tags_index": 0})),
        )
        .await
        .expect("drop");
    let s: SpecActorState = serde_json::from_slice(&state).expect("deser");
    assert_eq!(s.lists.get("tags").cloned().unwrap_or_default(), vec!["b"]);
}
