use super::*;

#[tokio::test]
async fn typed_uint64_refuses_before_postgres_adapter_state_or_effects_change() {
    let source = STRICT.replace(
        r#"params = ["desired", "expected_desired", "user_prompt"]"#,
        r#"params = ["desired", "expected_desired", "user_prompt", {name="count",type="uint64"}]"#,
    );
    let actor = actor(&source);
    for raw in [false, true] {
        let mut state = actor.initial_state();
        let before = state.clone();
        for bad in [json!("7"), json!(-1), json!(7.5), json!(null)] {
            let ctx = context();
            let incoming = message(
                "StartProcess",
                json!({"desired":"release-b", "expected_desired":"release-a", "count":bad}),
                raw,
            );
            assert!(actor.handle(&ctx, &mut state, &incoming).await.is_err());
            assert_eq!(before, state);
            assert!(ctx.pending_tells.lock().await.is_empty());
        }
        let ctx = context();
        let incoming = message(
            "StartProcess",
            json!({"desired":"release-b", "expected_desired":"release-a", "count":7}),
            raw,
        );
        actor.handle(&ctx, &mut state, &incoming).await.unwrap();
        assert_eq!(ctx.pending_tells.lock().await.len(), 1);
    }
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
async fn parsed_actor_uses_one_automaton_for_initial_state_and_transitions() {
    let source = STRICT
        .replace("StartProcess", "ParsedStart")
        .replace("release-a", "parsed-initial");
    let automaton = temper_spec::parse_automaton(&source).unwrap();
    let actor = SpecDrivenActor::from_automaton(&automaton);
    let mut state = actor.initial_state();
    let initial: SpecActorState = serde_json::from_slice(&state).unwrap();
    assert_eq!(initial.fields["desired"], "parsed-initial");
    let incoming = message(
        "ParsedStart",
        json!({"desired":"release-b", "expected_desired":"parsed-initial"}),
        true,
    );
    actor
        .handle(&context(), &mut state, &incoming)
        .await
        .unwrap();
    let result: SpecActorState = serde_json::from_slice(&state).unwrap();
    assert_eq!(result.fields["desired"], "release-b");
}
