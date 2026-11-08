//! Deterministic simulation regression tests.

use super::*;

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
const LATE_DELIVERY_IOA: &str =
    include_str!("../../../../test-fixtures/specs/late_delivery.ioa.toml");

/// A fault-delayed action must not be applied where it is not enabled.
///
/// The simulator chooses an action that is legal in the actor's state *now*,
/// hands it to a scheduler that may delay it by up to 20 ticks, and applies
/// it on delivery -- by which time the actor has moved on. Production
/// re-evaluates a late write against the entity's current state and refuses
/// it with a 409, so an execution that applies one models nothing that can
/// happen, and any violation it reports is a phantom.
///
/// `late_delivery.ioa.toml` has no legal execution that breaks its
/// invariant, so this asserts on a property of the checker rather than of
/// the spec: every reported violation is a false positive by construction.
#[test]
fn delayed_actions_are_not_applied_outside_their_from_states() {
    let config = SimConfig {
        seed: 7,
        max_ticks: 400,
        num_actors: 3,
        max_actions_per_actor: 40,
        max_counter: 2,
        faults: FaultConfig::heavy(),
    };

    let results = run_multi_seed_simulation_from_ioa(LATE_DELIVERY_IOA, &config, 20).unwrap();

    let phantoms: Vec<_> = results
        .iter()
        .flat_map(|r| r.violations.iter().map(move |v| (r.seed, v)))
        .collect();
    assert!(
        phantoms.is_empty(),
        "no legal execution can break SealedMeansFlagClear, so these are \
         transitions applied outside their from-states: {phantoms:#?}"
    );
    assert!(
        results.iter().any(|r| r.total_transitions > 0),
        "fixture applied no transitions at all, so it proves nothing"
    );
}

/// Past a budget's deadline the model offers no actions, so a liveness
/// check would call every actor deadlocked.
#[test]
fn a_spent_budget_finds_no_liveness_violations() {
    let spec = "[automaton]\nname = \"Loop\"\nstates = [\"On\"]\ninitial = \"On\"\n\n\
                [[action]]\nname = \"Go\"\nkind = \"input\"\nfrom = [\"On\"]\n\n\
                [[liveness]]\nname = \"KeepsMoving\"\nfrom = [\"On\"]\nhas_actions = true\n";
    let mut model = build_model_from_ioa(spec, 2).unwrap();
    model.limit = Some(crate::model::ExploreLimit {
        elapsed: crate::budget::Elapsed::start(crate::budget::Clock::Wall),
        time: std::time::Duration::ZERO,
        states: 1,
        lifted: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        work: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1)),
        exhausted: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });
    let config = SimConfig {
        seed: 7,
        max_ticks: 20,
        num_actors: 2,
        max_actions_per_actor: 5,
        max_counter: 2,
        faults: FaultConfig::none(),
    };
    let result = run_simulation_impl(&model, &config);
    assert!(
        result.liveness_violations.is_empty(),
        "{:?}",
        result.liveness_violations
    );
}

#[test]
fn test_simulation_no_faults() {
    let config = SimConfig {
        seed: 42,
        max_ticks: 200,
        num_actors: 3,
        max_actions_per_actor: 15,
        max_counter: 2,
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
fn test_simulation_is_reproducible() {
    let config = SimConfig {
        seed: 999,
        max_ticks: 100,
        num_actors: 2,
        max_actions_per_actor: 10,
        max_counter: 2,
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
        faults: FaultConfig::none(),
    };

    let result = run_simulation_from_ioa(ORDER_IOA, &config).unwrap();
    assert_eq!(result.actor_final_states.len(), 2);

    let model = build_model_from_ioa(ORDER_IOA, config.max_counter).unwrap();

    for (id, state) in &result.actor_final_states {
        assert!(id.starts_with("entity-"));
        assert!(
            model.states.iter().any(|s| *s == state.status),
            "Status '{}' not in spec states {:?}",
            state.status,
            model.states
        );
    }
}
