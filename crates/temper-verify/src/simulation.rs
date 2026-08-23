//! Deterministic simulation testing (Level 2 of the verification cascade).
//!
//! Uses the SimScheduler from temper-runtime to run multi-actor scenarios
//! with fault injection and seed-based reproducibility.
//!
//! Inspired by FoundationDB's simulation testing and TigerBeetle's VOPR:
//! - All non-determinism is controlled by a seed
//! - Faults (message delay/drop/reorder, actor crash) are injected
//! - Any failure is reproducible by replaying the same seed
//! - Specification invariants are checked after every transition

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::scheduler::{
    DeterministicRng, FaultConfig, SimActorState, SimMessage, SimScheduler,
};

use stateright::Model;

use temper_spec::automaton::AssertCompareOp;

use crate::model::{
    InvariantKind, LivenessKind, TemperModel, TemperModelAction, TemperModelState,
    build_model_from_ioa,
};

/// Configuration for a simulation run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SimConfig {
    /// Seed for the PRNG (determines all non-determinism).
    pub seed: u64,
    /// Maximum ticks before stopping.
    pub max_ticks: u64,
    /// Number of entity actors to simulate.
    pub num_actors: usize,
    /// Maximum actions per actor before it stops.
    pub max_actions_per_actor: usize,
    /// Maximum counter value for bounded model checking.
    pub max_counter: usize,
    /// Fault injection configuration.
    pub faults: FaultConfig,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            max_ticks: 500,
            num_actors: 3,
            max_actions_per_actor: 20,
            max_counter: 2,
            faults: FaultConfig::none(),
        }
    }
}

impl SimConfig {
    /// Create config with light faults.
    pub fn with_light_faults(mut self) -> Self {
        self.faults = FaultConfig::light();
        self
    }

    /// Create config with heavy faults.
    pub fn with_heavy_faults(mut self) -> Self {
        self.faults = FaultConfig::heavy();
        self
    }

    /// Set the seed.
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }
}

/// Result of a simulation run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SimulationResult {
    /// Whether all invariants held throughout the simulation.
    pub all_invariants_held: bool,
    /// Total ticks executed.
    pub ticks: u64,
    /// Total transitions applied across all actors.
    pub total_transitions: u64,
    /// Total messages sent.
    pub total_messages: u64,
    /// Total messages dropped (by fault injection).
    pub total_dropped: u64,
    /// Any invariant violations found.
    pub violations: Vec<InvariantViolation>,
    /// Any liveness violations found.
    pub liveness_violations: Vec<LivenessViolation>,
    /// The seed used (for replay).
    pub seed: u64,
    /// Per-actor final states.
    pub actor_final_states: Vec<(String, TemperModelState)>,
}

/// A liveness violation found during or after simulation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LivenessViolation {
    /// Which actor.
    pub actor_id: String,
    /// Which liveness property was violated.
    pub property: String,
    /// Description of the violation.
    pub description: String,
    /// The actor's final state.
    pub final_state: TemperModelState,
}

/// An invariant violation found during simulation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InvariantViolation {
    /// Which actor.
    pub actor_id: String,
    /// What action triggered it.
    pub action: String,
    /// The state before the action.
    pub state_before: TemperModelState,
    /// The state after the action.
    pub state_after: TemperModelState,
    /// Which invariant was violated.
    pub invariant: String,
    /// At what tick.
    pub tick: u64,
}

/// Run a deterministic simulation from I/O Automaton TOML source.
///
/// Returns an error if the IOA TOML fails to parse.
pub fn run_simulation_from_ioa(
    ioa_toml: &str,
    config: &SimConfig,
) -> Result<SimulationResult, String> {
    let model = build_model_from_ioa(ioa_toml, config.max_counter)?;
    Ok(run_simulation_impl(&model, config))
}

/// Run simulation across multiple seeds from I/O Automaton TOML source.
///
/// Returns an error if the IOA TOML fails to parse.
pub fn run_multi_seed_simulation_from_ioa(
    ioa_toml: &str,
    base_config: &SimConfig,
    num_seeds: u64,
) -> Result<Vec<SimulationResult>, String> {
    let model = build_model_from_ioa(ioa_toml, base_config.max_counter)?;
    Ok((0..num_seeds)
        .map(|i| {
            let mut config = base_config.clone();
            config.seed = base_config.seed.wrapping_add(i);
            run_simulation_impl(&model, &config)
        })
        .collect())
}

fn run_simulation_impl(model: &TemperModel, config: &SimConfig) -> SimulationResult {
    let mut sched = SimScheduler::new(config.seed, config.faults.clone());
    let mut rng = DeterministicRng::new(config.seed.wrapping_add(1));

    // Initialize actors
    let mut actor_states: Vec<(String, TemperModelState)> = Vec::new();
    let mut actor_action_counts: Vec<usize> = Vec::new();
    let mut visited_statuses: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for i in 0..config.num_actors {
        let actor_id = format!("entity-{i}");
        sched.register_actor(&actor_id);
        let initial = model.init_states()[0].clone();
        visited_statuses
            .entry(actor_id.clone())
            .or_default()
            .insert(initial.status.clone());
        actor_states.push((actor_id, initial));
        actor_action_counts.push(0);
    }

    let mut run = ModelRunState {
        actor_states,
        actor_action_counts,
        violations: Vec::new(),
        total_transitions: 0,
        visited_statuses,
    };
    let mut total_messages: u64 = 0;

    // Main simulation loop
    for tick in 0..config.max_ticks {
        if run.actor_states.is_empty() {
            break;
        }

        let actor_idx = rng.next_bound(run.actor_states.len());
        let (ref actor_id, ref current_state) = run.actor_states[actor_idx];

        if run.actor_action_counts[actor_idx] >= config.max_actions_per_actor {
            continue;
        }

        // One in-flight action per actor keeps action selection serialized to
        // the state that will consume it, matching actor mailbox semantics.
        if sched.has_in_flight(actor_id) {
            sched.tick();
            for msg in &sched.drain_ready() {
                apply_to_model(model, &mut run, tick, msg);
            }
            sched.finish_tick();
            continue;
        }

        if sched.actor_state(actor_id) == Some(&SimActorState::Crashed) {
            // A crashed idle actor still consumes logical ticks so the
            // scheduler can exercise its independent restart fault edge.
            sched.tick();
            for msg in &sched.drain_ready() {
                apply_to_model(model, &mut run, tick, msg);
            }
            sched.finish_tick();
            continue;
        }

        let mut valid_actions = Vec::new();
        model.actions(current_state, &mut valid_actions);

        if valid_actions.is_empty() {
            continue;
        }

        let action_idx = rng.next_bound(valid_actions.len());
        let action = valid_actions[action_idx].clone();

        let action_name = action.name.clone();
        sched.send(
            "sim-driver",
            actor_id,
            &action_name,
            &serde_json::to_string(&action).unwrap_or_default(),
        );
        total_messages += 1;

        sched.tick();
        for msg in &sched.drain_ready() {
            apply_to_model(model, &mut run, tick, msg);
        }
        sched.finish_tick();
    }

    // Flush delay-faulted tail deliveries through the same exactly-once path.
    let mut flush_budget = config.max_ticks;
    let mut tick = config.max_ticks.saturating_sub(1);
    while !sched.is_quiescent() && flush_budget > 0 {
        flush_budget -= 1;
        tick = tick.saturating_add(1);
        sched.tick();
        for msg in &sched.drain_ready() {
            apply_to_model(model, &mut run, tick, msg);
        }
        sched.finish_tick();
    }
    if !sched.is_quiescent() {
        run.violations.push(InvariantViolation {
            actor_id: "sim-driver".to_string(),
            action: "Flush".to_string(),
            state_before: model.init_states()[0].clone(),
            state_after: model.init_states()[0].clone(),
            invariant: format!(
                "simulation did not quiesce within {} flush ticks",
                config.max_ticks
            ),
            tick,
        });
    }

    // Post-simulation liveness checks
    let liveness_violations =
        check_liveness_post_simulation(model, &run.actor_states, &run.visited_statuses);

    SimulationResult {
        all_invariants_held: run.violations.is_empty(),
        ticks: sched.current_time(),
        total_transitions: run.total_transitions,
        total_messages,
        total_dropped: sched.total_dropped() as u64,
        violations: run.violations,
        liveness_violations,
        seed: config.seed,
        actor_final_states: run.actor_states,
    }
}

/// Post-simulation liveness checks.
///
/// - **NoDeadlock**: Each actor in a "from" state must have at least one valid action.
/// - **ReachesState**: Each actor must have reached one of the target states by simulation end.
///   (Weaker than Stateright's exhaustive BFS, but catches stuck actors.)
fn check_liveness_post_simulation(
    model: &TemperModel,
    actor_states: &[(String, TemperModelState)],
    visited_statuses: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<LivenessViolation> {
    let mut violations = Vec::new();

    for (actor_id, final_state) in actor_states {
        for live in &model.liveness {
            match &live.kind {
                LivenessKind::NoDeadlock { from } => {
                    if from.contains(&final_state.status) {
                        let mut actions = Vec::new();
                        model.actions(final_state, &mut actions);
                        if actions.is_empty() {
                            violations.push(LivenessViolation {
                                actor_id: actor_id.clone(),
                                property: live.name.clone(),
                                description: format!(
                                    "deadlock: actor in state '{}' has no enabled actions",
                                    final_state.status
                                ),
                                final_state: final_state.clone(),
                            });
                        }
                    }
                }
                LivenessKind::ReachesState { from, targets } => {
                    if targets.is_empty() {
                        continue;
                    }
                    let started_from = from.is_empty() || from.contains(&model.initial_status);
                    let visited_target = visited_statuses
                        .get(actor_id)
                        .is_some_and(|seen| targets.iter().any(|target| seen.contains(target)));
                    if started_from && !visited_target {
                        violations.push(LivenessViolation {
                            actor_id: actor_id.clone(),
                            property: live.name.clone(),
                            description: format!(
                                "actor never reached target states {:?}, ending at '{}'",
                                targets, final_state.status
                            ),
                            final_state: final_state.clone(),
                        });
                    }
                }
            }
        }
    }

    violations
}

/// Mutable state shared by the main driver and its bounded tail flush.
struct ModelRunState {
    actor_states: Vec<(String, TemperModelState)>,
    actor_action_counts: Vec<usize>,
    violations: Vec<InvariantViolation>,
    total_transitions: u64,
    visited_statuses: BTreeMap<String, BTreeSet<String>>,
}

/// Apply one mailbox-owned message to the verifier model exactly once.
fn apply_to_model(model: &TemperModel, run: &mut ModelRunState, tick: u64, msg: &SimMessage) {
    let Some(idx) = run
        .actor_states
        .iter()
        .position(|(actor_id, _)| actor_id == &msg.to)
    else {
        record_model_delivery_failure(
            model,
            run,
            tick,
            msg,
            "scheduled message targeted an unknown actor".to_string(),
        );
        return;
    };
    let Ok(action) = serde_json::from_str::<TemperModelAction>(&msg.payload) else {
        record_model_delivery_failure(
            model,
            run,
            tick,
            msg,
            "scheduled message payload was not a model action".to_string(),
        );
        return;
    };
    let (target_id, state_before) = &run.actor_states[idx];
    let mut enabled_actions = Vec::new();
    model.actions(state_before, &mut enabled_actions);
    if !enabled_actions.contains(&action) {
        let status = state_before.status.clone();
        run.violations.push(InvariantViolation {
            actor_id: target_id.clone(),
            action: action.name,
            state_before: state_before.clone(),
            state_after: state_before.clone(),
            invariant: format!("scheduled model action was rejected from status '{status}'"),
            tick,
        });
        return;
    }
    let Some(new_state) = model.next_state(state_before, action.clone()) else {
        let status = state_before.status.clone();
        run.violations.push(InvariantViolation {
            actor_id: target_id.clone(),
            action: action.name,
            state_before: state_before.clone(),
            state_after: state_before.clone(),
            invariant: format!("scheduled model action was rejected from status '{status}'"),
            tick,
        });
        return;
    };

    check_invariants_on_state(
        model,
        target_id,
        &action.name,
        state_before,
        &new_state,
        tick,
        &mut run.violations,
    );
    run.actor_states[idx].1 = new_state;
    run.visited_statuses
        .entry(run.actor_states[idx].0.clone())
        .or_default()
        .insert(run.actor_states[idx].1.status.clone());
    run.actor_action_counts[idx] += 1;
    run.total_transitions += 1;
}

fn record_model_delivery_failure(
    model: &TemperModel,
    run: &mut ModelRunState,
    tick: u64,
    msg: &SimMessage,
    invariant: String,
) {
    let state = model.init_states()[0].clone();
    run.violations.push(InvariantViolation {
        actor_id: msg.to.clone(),
        action: msg.msg_type.clone(),
        state_before: state.clone(),
        state_after: state,
        invariant,
        tick,
    });
}

/// Check invariants on a state using the model's resolved invariants.
///
/// All invariant data comes from the spec — no hardcoded entity knowledge.
fn check_invariants_on_state(
    model: &TemperModel,
    actor_id: &str,
    action_name: &str,
    state_before: &TemperModelState,
    state_after: &TemperModelState,
    tick: u64,
    violations: &mut Vec<InvariantViolation>,
) {
    // TypeInvariant: status must be in valid state set
    if !model.states.contains(&state_after.status) {
        violations.push(InvariantViolation {
            actor_id: actor_id.to_string(),
            action: action_name.to_string(),
            state_before: state_before.clone(),
            state_after: state_after.clone(),
            invariant: "TypeInvariant: status not in valid states".to_string(),
            tick,
        });
    }

    // Check each resolved invariant from the spec
    for inv in &model.invariants {
        let triggered =
            inv.trigger_states.is_empty() || inv.trigger_states.contains(&state_after.status);
        if !triggered {
            continue;
        }

        let violated = sim_kind_violated(&inv.kind, &inv.required_states, model, state_after);

        if violated {
            violations.push(InvariantViolation {
                actor_id: actor_id.to_string(),
                action: action_name.to_string(),
                state_before: state_before.clone(),
                state_after: state_after.clone(),
                invariant: inv.name.clone(),
                tick,
            });
        }
    }
}

/// Evaluate whether an [`InvariantKind`] is violated given model+state.
///
/// Pure recursion over compound variants; does not consult `trigger_states`.
fn sim_kind_violated(
    kind: &InvariantKind,
    required_states: &[String],
    model: &TemperModel,
    state_after: &TemperModelState,
) -> bool {
    match kind {
        InvariantKind::StatusInSet => !model.states.contains(&state_after.status),
        InvariantKind::CounterPositive { var } => {
            state_after.counters.get(var).copied().unwrap_or(0) == 0
        }
        InvariantKind::BoolRequired { var, expect } => {
            state_after.booleans.get(var).copied().unwrap_or(false) != *expect
        }
        InvariantKind::NoFurtherTransitions => {
            let mut actions = Vec::new();
            model.actions(state_after, &mut actions);
            !actions.is_empty()
        }
        InvariantKind::Implication => {
            let valid: Vec<&String> = required_states
                .iter()
                .filter(|s| model.states.contains(s))
                .collect();
            !valid.is_empty() && !valid.contains(&&state_after.status)
        }
        InvariantKind::CounterCompare { var, op, value } => {
            let val = state_after.counters.get(var).copied().unwrap_or(0);
            let holds = match op {
                AssertCompareOp::Gt => val > *value,
                AssertCompareOp::Gte => val >= *value,
                AssertCompareOp::Lt => val < *value,
                AssertCompareOp::Lte => val <= *value,
                AssertCompareOp::Eq => val == *value,
            };
            !holds
        }
        InvariantKind::NeverState { state } => state_after.status == *state,
        InvariantKind::And(parts) => parts
            .iter()
            .any(|k| sim_kind_violated(k, required_states, model, state_after)),
        InvariantKind::Or(parts) => parts
            .iter()
            .all(|k| sim_kind_violated(k, required_states, model, state_after)),
        InvariantKind::Unverifiable { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

    const CYCLIC_IOA: &str = r#"
[automaton]
name = "Loop"
states = ["Open", "Resolved"]
initial = "Open"

[[action]]
name = "Resolve"
kind = "input"
from = ["Open"]
to = "Resolved"

[[action]]
name = "Reopen"
kind = "input"
from = ["Resolved"]
to = "Open"

[[liveness]]
name = "EventuallyResolved"
from = ["Open"]
reaches = ["Resolved"]
"#;

    const UNREACHABLE_IOA: &str = r#"
[automaton]
name = "Stuck"
states = ["Open", "Parked", "Resolved"]
initial = "Open"

[[action]]
name = "Park"
kind = "input"
from = ["Open"]
to = "Parked"

[[action]]
name = "Unpark"
kind = "input"
from = ["Parked"]
to = "Open"

[[liveness]]
name = "EventuallyResolved"
from = ["Open"]
reaches = ["Resolved"]
"#;

    fn liveness_config(seed: u64) -> SimConfig {
        SimConfig {
            seed,
            max_ticks: 200,
            num_actors: 2,
            max_actions_per_actor: 20,
            max_counter: 2,
            faults: FaultConfig::none(),
        }
    }

    fn model_run(model: &TemperModel) -> ModelRunState {
        let actor_id = "entity-0".to_string();
        let initial = model.init_states()[0].clone();
        ModelRunState {
            actor_states: vec![(actor_id.clone(), initial.clone())],
            actor_action_counts: vec![0],
            violations: Vec::new(),
            total_transitions: 0,
            visited_statuses: BTreeMap::from([(actor_id, BTreeSet::from([initial.status]))]),
        }
    }

    fn model_message(target: &str, payload: String) -> SimMessage {
        SimMessage {
            from: "driver".to_string(),
            to: target.to_string(),
            msg_type: "model-action".to_string(),
            payload,
            deliver_at: 1,
            id: 1,
        }
    }

    #[test]
    fn verifier_records_unknown_and_malformed_deliveries() {
        let model = build_model_from_ioa(CYCLIC_IOA, 2).unwrap();
        let mut run = model_run(&model);

        apply_to_model(
            &model,
            &mut run,
            1,
            &model_message("missing", "{}".to_string()),
        );
        apply_to_model(
            &model,
            &mut run,
            2,
            &model_message("entity-0", "not-json".to_string()),
        );

        assert_eq!(run.violations.len(), 2);
        assert!(run.violations[0].invariant.contains("unknown actor"));
        assert!(run.violations[1].invariant.contains("not a model action"));
    }

    #[test]
    fn verifier_records_action_rejected_after_state_changes() {
        let model = build_model_from_ioa(CYCLIC_IOA, 2).unwrap();
        let mut actions = Vec::new();
        model.actions(&model.init_states()[0], &mut actions);
        let resolve = actions
            .into_iter()
            .find(|action| action.name == "Resolve")
            .unwrap();
        let message = model_message("entity-0", serde_json::to_string(&resolve).unwrap());
        let mut run = model_run(&model);

        apply_to_model(&model, &mut run, 1, &message);
        apply_to_model(&model, &mut run, 2, &message);

        assert_eq!(run.total_transitions, 1);
        assert_eq!(run.violations.len(), 1);
        assert!(run.violations[0].invariant.contains("was rejected"));
    }

    #[test]
    fn reaches_liveness_is_satisfied_by_a_trace_visit() {
        for seed in [7, 21, 99] {
            let result = run_simulation_from_ioa(CYCLIC_IOA, &liveness_config(seed)).unwrap();
            assert!(result.total_transitions > 0);
            assert!(
                result
                    .liveness_violations
                    .iter()
                    .all(|violation| violation.property != "EventuallyResolved"),
                "seed {seed}: {:?}",
                result.liveness_violations
            );
        }
    }

    #[test]
    fn reaches_liveness_fails_when_target_is_never_visited() {
        let result = run_simulation_from_ioa(UNREACHABLE_IOA, &liveness_config(7)).unwrap();
        assert!(
            result
                .liveness_violations
                .iter()
                .any(|violation| violation.property == "EventuallyResolved")
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
                model.states.contains(&state.status),
                "Status '{}' not in spec states {:?}",
                state.status,
                model.states
            );
        }
    }
}
