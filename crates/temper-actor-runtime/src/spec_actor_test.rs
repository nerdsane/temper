//! Tests for `SpecDrivenActor`, including the ARN-247 BLOCKER 1 pg-actor boundary.

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

#[test]
fn test_spec_driven_actor_initial_state() {
    let actor = SpecDrivenActor::from_ioa(SIMPLE_SPEC, HashMap::new()).unwrap();
    let state_bytes = actor.initial_state();
    let state: SpecActorState = serde_json::from_slice(&state_bytes).unwrap();
    assert_eq!(state.status, "Idle");
    assert_eq!(state.counters.get("rounds"), Some(&0usize));
}

const PARAM_SPEC: &str = r#"
[automaton]
name = "WorkSummary"
states = ["Done"]
initial = "Done"

[[state]]
name = "goal"
type = "string"
initial = ""

[[action]]
name = "AttachVector"
kind = "input"
from = ["Done"]
to = "Done"
params = ["semantic_vector"]
"#;

fn spec_message(action: &str, params: serde_json::Value) -> Message {
    let spec_msg = SpecMessage::with_params(action, params);
    Message {
        id: 1,
        from: None,
        to: ActorHandle::new("ns", "WorkSummary"),
        message_type: "SpecMessage".to_string(),
        payload: prost::Message::encode_to_vec(&spec_msg),
        correlation_id: None,
        created_at: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(), // determinism-ok: test fixture
    }
}

fn seeded_state(actor: &SpecDrivenActor) -> Vec<u8> {
    let mut state: SpecActorState = serde_json::from_slice(&actor.initial_state()).unwrap();
    // Seed a "frozen" field, forcing `fields` to a proper object.
    state.fields = serde_json::json!({ "goal": "original" });
    serde_json::to_vec(&state).unwrap()
}

// ARN-247 BLOCKER 1: the pg-actor runtime must enforce the declared-parameter
// boundary too — undeclared params are dropped, and a failed/unknown action
// never persists the merged params.
#[tokio::test]
async fn pg_actor_drops_undeclared_params_on_a_valid_action() {
    let actor = SpecDrivenActor::from_ioa(PARAM_SPEC, HashMap::new()).unwrap();
    let ctx = ActorContext::new(ActorHandle::new("ns", "WorkSummary"), None, None);
    let mut state = seeded_state(&actor);

    let message = spec_message(
        "AttachVector",
        serde_json::json!({ "semantic_vector": "[0.1]", "goal": "HIJACKED" }),
    );
    actor.handle(&ctx, &mut state, &message).await.unwrap();

    let after: SpecActorState = serde_json::from_slice(&state).unwrap();
    assert_eq!(after.fields["semantic_vector"], "[0.1]");
    assert_eq!(
        after.fields["goal"], "original",
        "undeclared goal was dropped"
    );
}

#[tokio::test]
async fn pg_actor_does_not_persist_smuggled_field_on_unknown_action() {
    let actor = SpecDrivenActor::from_ioa(PARAM_SPEC, HashMap::new()).unwrap();
    let ctx = ActorContext::new(ActorHandle::new("ns", "WorkSummary"), None, None);
    let mut state = seeded_state(&actor);
    let before = state.clone();

    // An unknown action name is the amplifier: pre-fix it still merged +
    // persisted the smuggled field. It must now be a no-op on state.
    let message = spec_message("GhostAction", serde_json::json!({ "goal": "HIJACKED" }));
    actor.handle(&ctx, &mut state, &message).await.unwrap();

    assert_eq!(
        state, before,
        "unknown action must not persist any state change"
    );
    let after: SpecActorState = serde_json::from_slice(&state).unwrap();
    assert_eq!(after.fields["goal"], "original");
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
