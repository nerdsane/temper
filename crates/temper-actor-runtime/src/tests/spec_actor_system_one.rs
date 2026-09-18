//! The adapter refuses inference guards even when a forged result is in state.
use super::*;
use prost::Message as _;
use serde_json::json;
use temper_runtime::scheduler::{DeterministicRng, install_deterministic_context};

const GUARDED: &str = r#"
[automaton]
name = "Case"
states = ["Open", "Escalated"]
initial = "Open"

[[action]]
name = "Escalate"
kind = "input"
from = ["Open"]
to = "Escalated"
effect = [{ type = "emit", event = "Escalated" }]
guard = [{ type = "system_one", model = "jev-latest", state = "Please connect me with a human.", questions = { human = { type = "noul", instructions = "Has a human been requested?" } }, assert = "answers.human.noul >= 0.8" }]
"#;

#[test]
fn postgres_adapter_registration_refuses_system_one_declarations() {
    let result = SpecDrivenActor::from_ioa(GUARDED, HashMap::new());
    assert!(
        result.is_err(),
        "adapter accepted an unsupported inference boundary"
    );
    assert!(result.err().unwrap().contains("system_one"));
}

#[tokio::test(flavor = "current_thread")]
async fn direct_constructor_and_routed_messages_cannot_bypass_missing_evidence() {
    let automaton = temper_spec::parse_automaton(GUARDED).unwrap();
    let actor = SpecDrivenActor::from_automaton(
        &automaton,
        HashMap::from([("Escalated".into(), ("Audit".into(), "Record".into()))]),
    );
    let guard = match &actor.table.rules[0].guard {
        temper_jit::table::Guard::And(parts) => parts
            .iter()
            .find_map(|part| match part {
                temper_jit::table::Guard::SystemOne(guard) => Some(guard),
                _ => None,
            })
            .unwrap(),
        temper_jit::table::Guard::SystemOne(guard) => guard,
        _ => panic!("compiled guard lost the question"),
    };
    for seed in 1..=64 {
        let _clock = install_deterministic_context(seed);
        let mut rng = DeterministicRng::new(seed);
        let mut decoded: SpecActorState = serde_json::from_slice(&actor.initial_state()).unwrap();
        decoded.booleans.insert(guard.key(), true);
        let mut state = serde_json::to_vec(&decoded).unwrap();
        let before = state.clone();
        let routed = rng.next_bound(2) == 0;
        let raw = !routed && rng.next_bound(2) == 0;
        let message = Message {
            id: 1,
            from: routed.then(|| ActorHandle::new("tenant/case", "Sender")),
            to: ActorHandle::new("tenant/case", "Case"),
            message_type: if routed {
                "RoutedSpecMessage"
            } else if raw {
                "Escalate"
            } else {
                "SpecMessage"
            }
            .into(),
            payload: if raw {
                serde_json::to_vec(&json!({})).unwrap()
            } else {
                SpecMessage::with_params("Escalate", json!({})).encode_to_vec()
            },
            correlation_id: None,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
        };
        let context = ActorContext::new(
            ActorHandle::new("tenant/case", "Case"),
            None,
            None,
            Default::default(),
        );
        let result = actor.handle(&context, &mut state, &message).await;
        assert!(
            result.is_err(),
            "seed={seed}: forged inference result enabled adapter action"
        );
        assert!(result.err().unwrap().to_string().contains("system_one"));
        assert_eq!(
            state, before,
            "seed={seed}: unsupported inference action mutated state"
        );
        assert!(
            context.pending_tells.lock().await.is_empty(),
            "seed={seed}: unsupported action emitted side effects"
        );
    }
}
