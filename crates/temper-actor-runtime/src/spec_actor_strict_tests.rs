//! Strict adapter tests execute the production handler with seeded requests.
use super::*;
use prost::Message as _;
use serde_json::json;
use temper_runtime::scheduler::DeterministicRng;

const STRICT: &str = r#"
[automaton]
name = "Process"
states = ["Idle", "Busy"]
initial = "Idle"
strict_action_params = true

[[state]]
name = "desired"
type = "string"
initial = "release-a"

[[state]]
name = "rounds"
type = "counter"
initial = "0"

[[state]]
name = "enabled"
type = "bool"
initial = "TRUE"

[[state]]
name = "members"
type = "list"
initial = '["first"]'

[[action]]
name = "StartProcess"
kind = "input"
from = ["Idle"]
to = "Busy"
params = ["desired", "expected_desired", "user_prompt"]
effect = [{type = "increment", var = "rounds"}, {type = "emit", event = "Processed"}]
[[action.constraints]]
kind = "param_equals_field"
param = "expected_desired"
field = "desired"

[[action]]
name = "SendInput"
kind = "input"
from = ["Busy"]
to = "Idle"
params = ["desired", "expected_desired", "user_prompt"]
effect = [{type = "increment", var = "rounds"}, {type = "emit", event = "Processed"}]
[[action.constraints]]
kind = "param_equals_field"
param = "expected_desired"
field = "desired"

[[action]]
name = "Noop"
kind = "input"
from = ["Idle", "Busy"]
params = []
"#;

fn actor(source: &str) -> SpecDrivenActor {
    SpecDrivenActor::from_ioa(
        source,
        HashMap::from([("Processed".into(), ("Audit".into(), "Record".into()))]),
    )
    .unwrap()
}

fn message(action: &str, params: serde_json::Value, raw: bool) -> Message {
    Message {
        id: 1,
        from: None,
        to: ActorHandle::new("strict-test", "Process"),
        message_type: if raw {
            action.into()
        } else {
            "SpecMessage".into()
        },
        payload: if raw {
            serde_json::to_vec(&params).unwrap()
        } else {
            SpecMessage::with_params(action, params).encode_to_vec()
        },
        correlation_id: None,
        created_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
    }
}

fn context() -> ActorContext {
    ActorContext::new(ActorHandle::new("strict-test", "Process"), None, None)
}

#[test]
fn strict_initial_values_use_the_shared_typed_declarations() {
    let state: SpecActorState = serde_json::from_slice(&actor(STRICT).initial_state()).unwrap();
    assert_eq!(state.fields["desired"], "release-a");
    assert_eq!(state.counters["rounds"], 0);
    assert!(state.booleans["enabled"]);
    assert_eq!(state.lists["members"], ["first"]);
}

#[tokio::test]
async fn strict_adapter_preserves_state_and_effects_on_rejection_across_seeded_sequences() {
    let actor = actor(STRICT).with_input_field_resets(
        ["StartProcess", "SendInput"]
            .into_iter()
            .map(|action| (action.into(), vec!["response".into()]))
            .collect(),
    );
    for seed in 1..=64 {
        let mut rng = DeterministicRng::new(seed);
        let mut state = actor.initial_state();
        let mut desired = String::from("release-a");
        let mut busy = false;
        let mut rounds = 0;
        for step in 0..48 {
            // Scratch data must survive rejection, but be cleared by accepted user turns.
            let mut decoded: SpecActorState = serde_json::from_slice(&state).unwrap();
            decoded.fields["response"] = json!(format!("previous-{seed}-{step}"));
            state = serde_json::to_vec(&decoded).unwrap();
            let before = state.clone();
            let choice = rng.next_bound(8);
            let next = format!("release-{}", rng.next_u64());
            let mut params = json!({"desired": next, "expected_desired": desired, "user_prompt": format!("turn-{}", rng.next_u64())});
            let mut action = if busy { "SendInput" } else { "StartProcess" };
            match choice {
                2 => {
                    params["hidden"] = json!(rng.next_u64());
                }
                3 => {
                    params["expected_desired"] = json!(format!("stale-{step}"));
                }
                4 => {
                    params = json!([next]);
                }
                5 => {
                    action = if busy { "StartProcess" } else { "SendInput" };
                }
                6 => {
                    action = "Unknown";
                }
                _ => {}
            }
            let mut incoming = message(action, params, choice == 1);
            if choice == 7 {
                incoming.payload = vec![0xff];
                incoming.message_type = "SpecMessage".into();
            }
            let ctx = context();
            let result = actor.handle(&ctx, &mut state, &incoming).await;
            if choice < 2 {
                assert!(result.is_ok(), "seed {seed} step {step}: {result:?}");
                rounds += 1;
                busy = !busy;
                desired = next;
                let tells = ctx.pending_tells.lock().await;
                assert_eq!(tells.len(), 1);
                let emitted = SpecMessage::decode(tells[0].payload.as_slice()).unwrap();
                let emitted_fields: serde_json::Value =
                    serde_json::from_slice(&emitted.params).unwrap();
                assert!(emitted_fields.get("response").is_none());
                assert_eq!(emitted_fields["desired"], desired);
                drop(tells);
                let after: SpecActorState = serde_json::from_slice(&state).unwrap();
                assert_eq!(after.fields["desired"], desired);
                assert_eq!(after.status, if busy { "Busy" } else { "Idle" });
                assert_eq!(after.counters["rounds"], rounds);
                assert!(after.fields.get("response").is_none());
            } else {
                assert!(
                    result.is_err(),
                    "accepted choice {choice} at seed {seed} step {step}"
                );
                assert_eq!(
                    state, before,
                    "rejection changed bytes at seed {seed} step {step}"
                );
                assert!(ctx.pending_tells.lock().await.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn empty_raw_input_is_an_empty_object_and_bad_json_is_rejected() {
    let actor = actor(STRICT);
    let mut state = actor.initial_state();
    let mut incoming = message("Noop", json!({}), true);
    incoming.payload.clear();
    actor
        .handle(&context(), &mut state, &incoming)
        .await
        .unwrap();
    for payload in [b"{".as_slice(), b"null", b"42", b"[]"] {
        incoming.payload = payload.to_vec();
        let before = state.clone();
        assert!(
            actor
                .handle(&context(), &mut state, &incoming)
                .await
                .is_err()
        );
        assert_eq!(before, state);
    }
}

#[tokio::test]
async fn legacy_denied_transition_does_not_merge_parameters_or_clear_scratch() {
    let actor = actor(&STRICT.replace(
        "strict_action_params = true",
        "strict_action_params = false",
    ));
    let mut decoded: SpecActorState = serde_json::from_slice(&actor.initial_state()).unwrap();
    decoded.fields = json!({"response": "retained", "desired": "release-a"});
    let mut state = serde_json::to_vec(&decoded).unwrap();
    let before = state.clone();
    let ctx = context();
    actor
        .handle(
            &ctx,
            &mut state,
            &message(
                "SendInput",
                json!({"desired": "bad", "expected_desired": "release-a"}),
                false,
            ),
        )
        .await
        .unwrap();
    assert_eq!(before, state);
    assert!(ctx.pending_tells.lock().await.is_empty());
}

#[cfg(test)]
mod tests {
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
}

#[test]
fn routed_messages_preserve_the_wire_contract_used_by_concrete_integration_actors() {
    let ordinary = SpecMessage::with_params("Execute", json!({"input":"test"}));
    let routed = RoutedSpecMessage::from(ordinary.clone());
    assert_eq!(routed.encode_to_vec(), ordinary.encode_to_vec());
    assert_eq!(
        SpecMessage::decode(routed.encode_to_vec().as_slice()).unwrap(),
        ordinary
    );
}

#[tokio::test]
async fn round_three_raw_non_strict_constrained_input_keeps_its_parameters() {
    let source = STRICT.replace(
        "strict_action_params = true",
        "strict_action_params = false",
    );
    let actor = actor(&source);
    let mut state = actor.initial_state();
    actor
        .handle(
            &context(),
            &mut state,
            &message(
                "StartProcess",
                json!({
                    "desired":"release-b", "expected_desired":"release-a", "user_prompt":"next"
                }),
                true,
            ),
        )
        .await
        .expect("valid constrained JSON input must execute");
    let after: SpecActorState = serde_json::from_slice(&state).unwrap();
    assert_eq!(after.fields["desired"], "release-b");
    assert_eq!(after.status, "Busy");
}

#[tokio::test]
async fn round_three_routed_null_source_means_no_generated_fields() {
    let actor = actor(STRICT);
    let mut state = actor.initial_state();
    let mut incoming = message("Noop", serde_json::Value::Null, false);
    incoming.message_type = "RoutedSpecMessage".into();
    incoming.from = Some(ActorHandle::new("strict-test", "Source"));
    actor
        .handle(&context(), &mut state, &incoming)
        .await
        .expect("legacy null source must reach a parameterless strict action");
    let before = state.clone();
    incoming.payload =
        SpecMessage::with_params("StartProcess", serde_json::Value::Null).encode_to_vec();
    assert!(
        actor
            .handle(&context(), &mut state, &incoming)
            .await
            .is_err()
    );
    assert_eq!(
        state, before,
        "null projection cannot invent a required input"
    );
}

#[tokio::test]
async fn round_three_unconfigured_process_preserves_declared_application_fields() {
    let actor = actor(STRICT);
    let mut decoded: SpecActorState = serde_json::from_slice(&actor.initial_state()).unwrap();
    decoded.fields["response"] = json!("persistent application response");
    let mut state = serde_json::to_vec(&decoded).unwrap();
    let ctx = context();
    actor
        .handle(
            &ctx,
            &mut state,
            &message(
                "StartProcess",
                json!({
                    "desired":"release-b", "expected_desired":"release-a", "user_prompt":"next"
                }),
                false,
            ),
        )
        .await
        .unwrap();
    let after: SpecActorState = serde_json::from_slice(&state).unwrap();
    assert_eq!(after.fields["response"], "persistent application response");
    let tells = ctx.pending_tells.lock().await;
    let emitted = SpecMessage::decode(tells[0].payload.as_slice()).unwrap();
    let fields: serde_json::Value = serde_json::from_slice(&emitted.params).unwrap();
    assert_eq!(fields["response"], "persistent application response");
}

#[test]
fn round_three_non_strict_constrained_fresh_state_materializes_all_declared_defaults() {
    let source = STRICT.replace(
        "strict_action_params = true",
        "strict_action_params = false",
    );
    let state: SpecActorState = serde_json::from_slice(&actor(&source).initial_state()).unwrap();
    assert_eq!(state.fields["desired"], "release-a");
    assert!(state.booleans["enabled"]);
    assert_eq!(state.lists["members"], ["first"]);
}

#[tokio::test]
async fn contracted_empty_persisted_bytes_are_not_fresh_creation() {
    for source in [
        STRICT.to_string(),
        STRICT.replace(
            "strict_action_params = true",
            "strict_action_params = false",
        ),
    ] {
        let actor = actor(&source);
        let mut empty = vec![];
        let ctx = context();
        let incoming = message("Noop", json!({}), false);
        assert!(matches!(
            actor.handle(&ctx, &mut empty, &incoming).await,
            Err(ActorError::Rejected(_))
        ));
        assert!(empty.is_empty());
        assert!(ctx.pending_tells.lock().await.is_empty());
        let mut initialized = actor.initial_state_for(&ActorHandle::new("valid", "Process"));
        actor
            .handle(&context(), &mut initialized, &incoming)
            .await
            .expect("supported creation persisted valid initial bytes");
    }
    let legacy = SpecDrivenActor::from_ioa(
        r#"
[automaton]
name = "Legacy"
states = ["Idle"]
initial = "Idle"
[[action]]
name = "Noop"
kind = "input"
from = ["Idle"]
"#,
        HashMap::new(),
    )
    .unwrap();
    let mut empty = vec![];
    legacy
        .handle(&context(), &mut empty, &message("Noop", json!({}), false))
        .await
        .unwrap();
    let recovered: SpecActorState = serde_json::from_slice(&empty).unwrap();
    assert_eq!(recovered.status, "Idle");
}

#[tokio::test]
async fn legacy_identity_and_accepted_inputs_remain_unchanged_but_refusals_do_not_mutate() {
    let source = STRICT.replace("strict_action_params = true", "strict_action_params = false")
        .replace("\n[[action.constraints]]\nkind = \"param_equals_field\"\nparam = \"expected_desired\"\nfield = \"desired\"\n", "");
    let actor = actor(&source);
    let mut state = actor.initial_state_for(&ActorHandle::new("legacy", "Process"));
    assert_eq!(
        state,
        actor.initial_state(),
        "legacy creation must not add identity"
    );
    let ctx = context();
    actor
        .handle(
            &ctx,
            &mut state,
            &message(
                "StartProcess",
                json!({
                    "desired":"accepted", "legacy_extra":"kept"
                }),
                false,
            ),
        )
        .await
        .unwrap();
    let accepted = state.clone();
    let parsed: SpecActorState = serde_json::from_slice(&state).unwrap();
    assert_eq!(parsed.fields["legacy_extra"], "kept");
    assert_eq!(parsed.status, "Busy");
    let tells = ctx.pending_tells.lock().await;
    assert_eq!(tells.len(), 1);
    let emitted = SpecMessage::decode(tells[0].payload.as_slice()).unwrap();
    let fields: serde_json::Value = serde_json::from_slice(&emitted.params).unwrap();
    assert_eq!(fields["legacy_extra"], "kept");
    drop(tells);
    for action in ["StartProcess", "Unknown"] {
        let ctx = context();
        actor
            .handle(
                &ctx,
                &mut state,
                &message(action, json!({"desired":"refused"}), false),
            )
            .await
            .unwrap();
        assert_eq!(state, accepted);
        assert!(ctx.pending_tells.lock().await.is_empty());
    }
}
