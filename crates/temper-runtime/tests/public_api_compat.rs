use temper_runtime::scheduler::SimActorResult;

#[test]
fn sim_actor_result_preserves_exhaustive_struct_literal() {
    let result = SimActorResult {
        all_invariants_held: true,
        seed: 42,
        transitions: 0,
        messages: 0,
        dropped: 0,
        violations: Vec::new(),
        actor_states: Vec::new(),
    };

    assert!(result.all_invariants_held);
}
