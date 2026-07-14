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

use temper_runtime::scheduler::{DeterministicRng, FaultConfig, SimActorState, SimScheduler};

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
    /// Maximum ready messages transferred from scheduler mailboxes per tick.
    pub message_budget_per_tick: usize,
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
            message_budget_per_tick: 1_024,
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
    assert!(
        config.message_budget_per_tick > 0,
        "message budget per tick must be positive"
    );
    let mailbox_budget = usize::try_from(config.max_ticks)
        .expect("maximum ticks must fit the platform address space")
        .max(1);
    let mut sched =
        SimScheduler::with_mailbox_budget(config.seed, config.faults.clone(), mailbox_budget);
    let mut rng = DeterministicRng::new(config.seed.wrapping_add(1));

    // Initialize actors
    let mut actor_states: Vec<(String, TemperModelState)> = Vec::new();
    let mut actor_action_counts: Vec<usize> = Vec::new();
    let mut actor_in_flight_actions: Vec<usize> = Vec::new();

    for i in 0..config.num_actors {
        let actor_id = format!("entity-{i}");
        sched.register_actor(&actor_id);
        let initial = model.init_states()[0].clone();
        actor_states.push((actor_id, initial));
        actor_action_counts.push(0);
        actor_in_flight_actions.push(0);
    }

    let mut violations = Vec::new();
    let mut total_transitions: u64 = 0;
    let mut total_messages: u64 = 0;
    let mut observed_scheduler_drops = 0;

    // Main simulation loop
    for tick in 0..config.max_ticks {
        if actor_states.is_empty() {
            break;
        }

        let actor_idx = rng.next_bound(actor_states.len());
        let actor_id = actor_states[actor_idx].0.clone();
        let can_schedule = actor_action_counts[actor_idx] + actor_in_flight_actions[actor_idx]
            < config.max_actions_per_actor
            && sched.actor_state(&actor_id) != Some(&SimActorState::Crashed);

        if can_schedule {
            let mut valid_actions = Vec::new();
            model.actions(&actor_states[actor_idx].1, &mut valid_actions);

            if !valid_actions.is_empty() {
                let action_idx = rng.next_bound(valid_actions.len());
                let action = valid_actions[action_idx].clone();
                sched.send(
                    "sim-driver",
                    &actor_id,
                    &action.name,
                    &serde_json::to_string(&action).unwrap_or_default(),
                );
                actor_in_flight_actions[actor_idx] += 1;
                total_messages += 1;
            }
        }

        // Logical time and ready delivery progress independently of whether a
        // new action was eligible this tick.
        sched.tick();
        for dropped in &sched.dropped_log()[observed_scheduler_drops..] {
            if dropped.from == "sim-driver"
                && let Some(idx) = actor_states.iter().position(|(id, _)| id == &dropped.to)
            {
                assert!(
                    actor_in_flight_actions[idx] > 0,
                    "dropped action must own a reservation"
                );
                actor_in_flight_actions[idx] -= 1;
            }
        }
        observed_scheduler_drops = sched.dropped_log().len();
        let delivered = sched.drain_ready(config.message_budget_per_tick);

        for msg in &delivered {
            let target_idx = actor_states.iter().position(|(id, _)| id == &msg.to);
            let Some(idx) = target_idx else { continue };

            assert!(
                actor_in_flight_actions[idx] > 0,
                "delivered action must own a reservation"
            );
            actor_in_flight_actions[idx] -= 1;

            let (ref target_id, ref state_before) = actor_states[idx];

            let action: TemperModelAction = match serde_json::from_str(&msg.payload) {
                Ok(a) => a,
                Err(_) => continue,
            };

            if let Some(new_state) = model.next_state(state_before, action.clone()) {
                check_invariants_on_state(
                    model,
                    target_id,
                    &action.name,
                    state_before,
                    &new_state,
                    tick,
                    &mut violations,
                );

                actor_states[idx].1 = new_state;
                actor_action_counts[idx] += 1;
                total_transitions += 1;
            }
        }
    }

    // Post-simulation liveness checks
    let liveness_violations = check_liveness_post_simulation(model, &actor_states);

    SimulationResult {
        all_invariants_held: violations.is_empty(),
        ticks: config.max_ticks.min(sched.current_time()),
        total_transitions,
        total_messages,
        total_dropped: sched.total_dropped() as u64,
        violations,
        liveness_violations,
        seed: config.seed,
        actor_final_states: actor_states,
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
                    // If the actor started from a "from" state, it should have
                    // reached a target state by the end of simulation.
                    let started_from = from.is_empty() || from.contains(&model.initial_status);
                    if started_from && !targets.contains(&final_state.status) {
                        violations.push(LivenessViolation {
                            actor_id: actor_id.clone(),
                            property: live.name.clone(),
                            description: format!(
                                "actor did not reach target states {:?}, stuck at '{}'",
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
#[path = "simulation/tests.rs"]
mod tests;
