//! Literal string effects through the production materialized actor adapter.
use super::*;
use prost::Message as _;
use serde_json::json;

fn context() -> ActorContext {
    ActorContext::new(
        ActorHandle::new("string-test", "StringRecord"),
        None,
        None,
        Default::default(),
    )
}

fn message(action: &str, params: serde_json::Value) -> Message {
    Message {
        id: 1,
        from: None,
        message_type: "SpecMessage".into(),
        payload: SpecMessage::with_params(action, params).encode_to_vec(),
        to: ActorHandle::new("string-test", "StringRecord"),
        correlation_id: None,
        created_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
    }
}

#[tokio::test]
async fn string_effect_clears_a_prior_action_parameter() {
    let source = include_str!("../../../../test-fixtures/specs/string_effect.ioa.toml");
    let source = source.replace("terminal = [\"Recovered\"]", "terminal = []")
        + r#"
[[action]]
name = "ReadClear"
from = ["Recovered"]
to = "Recovered"
guard = "error == '' && reason == 'recovered'"
"#;
    let actor = SpecDrivenActor::from_ioa(&source).unwrap();
    let mut state = actor.initial_state();
    for (action, params, expected) in [
        ("Fail", json!({"error": "stage failed"}), "stage failed"),
        ("Recover", json!({}), ""),
        ("ReadClear", json!({}), ""),
    ] {
        actor
            .handle(&context(), &mut state, &message(action, params))
            .await
            .unwrap();
        let decoded: SpecActorState = serde_json::from_slice(&state).unwrap();
        assert_eq!(decoded.fields["error"], expected);
    }
}

#[tokio::test]
async fn string_effect_accepts_legacy_non_object_params_without_panicking() {
    let source = include_str!("../../../../test-fixtures/specs/string_effect.ioa.toml");
    let actor = SpecDrivenActor::from_ioa(source).unwrap();
    let mut state = actor.initial_state();
    actor
        .handle(
            &context(),
            &mut state,
            &message("Fail", json!({"error": "failed"})),
        )
        .await
        .unwrap();
    actor
        .handle(&context(), &mut state, &message("Recover", json!([])))
        .await
        .unwrap();
    let decoded: SpecActorState = serde_json::from_slice(&state).unwrap();
    assert_eq!(decoded.fields["error"], "");
    assert_eq!(decoded.fields["reason"], "recovered");
}

#[tokio::test]
async fn string_parameters_override_effects_on_the_same_action() {
    let source = include_str!("../../../../test-fixtures/specs/string_effect_param_order.ioa.toml");
    for strict in [false, true] {
        let source = source.replace(
            "strict_action_params = true",
            &format!("strict_action_params = {strict}"),
        );
        let actor = SpecDrivenActor::from_ioa(&source).unwrap();
        let mut state = actor.initial_state();
        for (action, params, status) in [
            ("Write", json!({"error": "detail"}), "Written"),
            ("Read", json!({}), "Read"),
        ] {
            actor
                .handle(&context(), &mut state, &message(action, params))
                .await
                .unwrap();
            let decoded: SpecActorState = serde_json::from_slice(&state).unwrap();
            assert_eq!(
                decoded.fields["error"], "detail",
                "strict={strict}, {action}"
            );
            assert_eq!(
                decoded.fields["reason"], "last",
                "strict={strict}, {action}"
            );
            assert_eq!(decoded.status, status, "strict={strict}, {action}");
        }
    }
}
