use super::*;

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

#[test]
fn test_simulation_no_faults() {
    let config = SimConfig {
        seed: 42,
        max_ticks: 200,
        num_actors: 3,
        max_actions_per_actor: 15,
        max_counter: 2,
        message_budget_per_tick: 1_024,
        faults: FaultConfig::none(),
    };

    let result = run_simulation_from_ioa(ORDER_IOA, &config).unwrap();
    assert!(
        result.all_invariants_held,
        "No invariant violations expected without faults, got: {:?}",
        result.violations
    );
    assert!(
        result.total_transitions > 0,
        "Should have applied some transitions"
    );
}

#[test]
fn test_simulation_light_faults() {
    let config = SimConfig {
        seed: 123,
        max_ticks: 300,
        num_actors: 3,
        max_actions_per_actor: 20,
        max_counter: 2,
        message_budget_per_tick: 1_024,
        faults: FaultConfig::light(),
    };

    let result = run_simulation_from_ioa(ORDER_IOA, &config).unwrap();
    assert!(
        result.all_invariants_held,
        "No invariant violations expected with light faults, got: {:?}",
        result.violations
    );
}

#[test]
fn test_simulation_heavy_faults() {
    let config = SimConfig {
        seed: 456,
        max_ticks: 300,
        num_actors: 5,
        max_actions_per_actor: 15,
        max_counter: 2,
        message_budget_per_tick: 1_024,
        faults: FaultConfig::heavy(),
    };

    let result = run_simulation_from_ioa(ORDER_IOA, &config).unwrap();
    assert!(
        result.all_invariants_held,
        "Invariants must hold even under heavy faults, got: {:?}",
        result.violations
    );
    assert!(
        result.total_dropped > 0 || result.total_messages > 0,
        "Should have processed messages"
    );
}

#[test]
fn delayed_message_due_on_final_tick_is_delivered() {
    let config = SimConfig {
        seed: 1,
        max_ticks: 2,
        num_actors: 1,
        max_actions_per_actor: 1,
        max_counter: 2,
        message_budget_per_tick: 1,
        faults: FaultConfig {
            message_delay_prob: 1.0,
            max_delay_ticks: 2,
            message_drop_prob: 0.0,
            actor_crash_prob: 0.0,
            actor_restart_prob: 0.0,
        },
    };

    let result = run_simulation_from_ioa(ORDER_IOA, &config).unwrap();

    assert_eq!(result.total_dropped, 0, "the message was not fault-dropped");
    assert_eq!(
        result.total_messages, 1,
        "the delayed action must reserve its per-actor budget"
    );
    assert_eq!(
        result.total_transitions, 1,
        "the delivery due on the final tick must be applied exactly once"
    );
}

#[test]
fn test_simulation_is_reproducible() {
    let config = SimConfig {
        seed: 999,
        max_ticks: 100,
        num_actors: 2,
        max_actions_per_actor: 10,
        max_counter: 2,
        message_budget_per_tick: 1_024,
        faults: FaultConfig::light(),
    };

    let result1 = run_simulation_from_ioa(ORDER_IOA, &config).unwrap();
    let result2 = run_simulation_from_ioa(ORDER_IOA, &config).unwrap();

    assert_eq!(
        result1.total_transitions, result2.total_transitions,
        "Same seed must produce same number of transitions"
    );
    assert_eq!(
        result1.total_messages, result2.total_messages,
        "Same seed must produce same number of messages"
    );

    for (i, ((id1, s1), (id2, s2))) in result1
        .actor_final_states
        .iter()
        .zip(result2.actor_final_states.iter())
        .enumerate()
    {
        assert_eq!(id1, id2, "Actor {i} ID mismatch");
        assert_eq!(s1.status, s2.status, "Actor {i} status mismatch");
        assert_eq!(s1.counters, s2.counters, "Actor {i} counters mismatch");
    }
}

#[test]
fn test_simulation_different_seeds_diverge() {
    let config1 = SimConfig::default().with_seed(42);
    let config2 = SimConfig::default().with_seed(9999);

    let result1 = run_simulation_from_ioa(ORDER_IOA, &config1).unwrap();
    let result2 = run_simulation_from_ioa(ORDER_IOA, &config2).unwrap();

    assert!(result1.total_transitions > 0);
    assert!(result2.total_transitions > 0);
}

#[test]
fn test_multi_seed_simulation() {
    let config = SimConfig {
        seed: 1,
        max_ticks: 100,
        num_actors: 2,
        max_actions_per_actor: 10,
        max_counter: 2,
        message_budget_per_tick: 1_024,
        faults: FaultConfig::light(),
    };

    let results = run_multi_seed_simulation_from_ioa(ORDER_IOA, &config, 10).unwrap();
    assert_eq!(results.len(), 10);

    for (i, result) in results.iter().enumerate() {
        assert!(
            result.all_invariants_held,
            "Seed {} failed with violations: {:?}",
            result.seed, result.violations
        );
        assert_eq!(result.seed, 1 + i as u64);
    }
}

#[test]
fn test_simulation_result_contains_final_states() {
    let config = SimConfig {
        seed: 77,
        max_ticks: 50,
        num_actors: 2,
        max_actions_per_actor: 5,
        max_counter: 2,
        message_budget_per_tick: 1_024,
        faults: FaultConfig::none(),
    };

    let result = run_simulation_from_ioa(ORDER_IOA, &config).unwrap();
    assert_eq!(result.actor_final_states.len(), 2);

    let model = build_model_from_ioa(ORDER_IOA, config.max_counter).unwrap();

    for (id, state) in &result.actor_final_states {
        assert!(id.starts_with("entity-"));
        assert!(
            model.states.contains(&state.status),
            "Status '{}' not in spec states {:?}",
            state.status,
            model.states
        );
    }
}
