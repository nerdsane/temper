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

use temper_spec::predicate::Truth;

use crate::model::semantics::truth;

use crate::model::{
    LivenessKind, TemperModel, TemperModelAction, TemperModelState, build_model_from_ioa,
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
    /// Deliveries refused because the action was not enabled on arrival.
    ///
    /// A fault-delayed message can land after its target has changed state, by
    /// which point the spec may no longer permit the action. Those deliveries
    /// are skipped rather than applied, and counted here so the skipping is
    /// observable: a run that rejects nearly everything is exploring far less
    /// than its transition count suggests.
    ///
    /// `#[serde(default)]` so a result from an older server still decodes.
    #[serde(default)]
    pub total_rejected: u64,
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
    Ok(run_multi_seed_simulation(&model, base_config, num_seeds))
}

/// Run simulation across multiple seeds on a pre-built `TemperModel`.
pub fn run_multi_seed_simulation(
    model: &TemperModel,
    base_config: &SimConfig,
    num_seeds: u64,
) -> Vec<SimulationResult> {
    (0..num_seeds)
        .map(|i| {
            let mut config = base_config.clone();
            config.seed = base_config.seed.wrapping_add(i);
            run_simulation_impl(model, &config)
        })
        .collect()
}

fn run_simulation_impl(model: &TemperModel, config: &SimConfig) -> SimulationResult {
    let mut sched = SimScheduler::new(config.seed, config.faults.clone());
    let mut rng = DeterministicRng::new(config.seed.wrapping_add(1));

    // Initialize actors
    let mut actor_states: Vec<(String, TemperModelState)> = Vec::new();
    let mut actor_action_counts: Vec<usize> = Vec::new();
    // Each rule's pending obligation, updated in visit order. An earlier
    // target cannot satisfy a later visit to one of the rule's `from` places.
    let mut actor_reaches: Vec<Vec<bool>> = Vec::new();

    for i in 0..config.num_actors {
        let actor_id = format!("entity-{i}");
        sched.register_actor(&actor_id);
        let initial = model.init_states()[0].clone();
        let mut pending = vec![false; model.liveness.len()];
        visit_reaches(model, &mut pending, &initial.status, true);
        actor_reaches.push(pending);
        actor_states.push((actor_id, initial));
        actor_action_counts.push(0);
    }

    let mut violations = Vec::new();
    let mut total_transitions: u64 = 0;
    let mut total_messages: u64 = 0;
    let mut total_rejected: u64 = 0;

    // Main simulation loop
    for tick in 0..config.max_ticks {
        if actor_states.is_empty() {
            break;
        }

        let actor_idx = rng.next_bound(actor_states.len());
        let (ref actor_id, ref current_state) = actor_states[actor_idx];

        if actor_action_counts[actor_idx] >= config.max_actions_per_actor {
            continue;
        }

        if sched.actor_state(actor_id) == Some(&SimActorState::Crashed) {
            continue;
        }

        let mut valid_actions = Vec::new();
        model.actions(current_state, &mut valid_actions);

        if valid_actions.is_empty() {
            continue;
        }

        let action_idx = rng.next_bound(valid_actions.len());
        let action = valid_actions[action_idx].clone();

        let action_name = action.name;
        sched.send(
            "sim-driver",
            actor_id,
            &action_name,
            &serde_json::to_string(&action).unwrap_or_default(),
        );
        total_messages += 1;

        let delivered = sched.tick();

        for msg in &delivered {
            let target_idx = actor_states.iter().position(|(id, _)| id == &msg.to);
            let Some(idx) = target_idx else { continue };

            let (ref target_id, ref state_before) = actor_states[idx];

            let action: TemperModelAction = match serde_json::from_str(&msg.payload) {
                Ok(a) => a,
                Err(_) => continue,
            };

            // Re-evaluate the precondition against the state the actor is in
            // *now*, not the one it was in when the action was chosen. The
            // scheduler may have held this message for up to `max_delay_ticks`,
            // during which the actor moved on, and `next_state` resolves a
            // transition by name without rechecking anything. Applying it
            // anyway fabricates a transition the spec forbids, and the
            // invariant it breaks is reported as a real violation -- a phantom
            // that reads exactly like a genuine bug and costs a spec bisect to
            // dismiss. Production refuses a late write with a 409 for the same
            // reason, so counting it as rejected is the faithful model.
            if !model.action_enabled(state_before, &action) {
                total_rejected += 1;
                continue;
            }

            if let Some(new_state) = model.next_state(state_before, action.clone()) {
                check_invariants_on_state(
                    model,
                    target_id,
                    &action,
                    state_before,
                    &new_state,
                    tick,
                    &mut violations,
                );

                visit_reaches(model, &mut actor_reaches[idx], &new_state.status, false);
                actor_states[idx].1 = new_state;
                actor_action_counts[idx] += 1;
                total_transitions += 1;
            }
        }

        sched.tick();
    }

    // Post-simulation liveness checks
    let liveness_violations = check_liveness_post_simulation(model, &actor_states, &actor_reaches);

    SimulationResult {
        all_invariants_held: violations.is_empty(),
        ticks: config.max_ticks.min(sched.current_time()),
        total_transitions,
        total_messages,
        total_dropped: sched.total_dropped() as u64,
        total_rejected,
        violations,
        liveness_violations,
        seed: config.seed,
        actor_final_states: actor_states,
    }
}

/// A visit starts an obligation for each matching rule and satisfies it if
/// this place is also a target. Without `from`, only a new record starts one.
fn visit_reaches(model: &TemperModel, pending: &mut [bool], place: &str, initial: bool) {
    for (live, pending) in model.liveness.iter().zip(pending) {
        if let LivenessKind::ReachesState { from, targets } = &live.kind {
            if (from.is_empty() && initial) || from.iter().any(|from| from == place) {
                *pending = !targets.is_empty();
            }
            if targets.iter().any(|target| target == place) {
                *pending = false;
            }
        }
    }
}

/// Post-simulation liveness checks.
///
/// - **NoDeadlock**: Each actor in a "from" state must have at least one valid action.
/// - **ReachesState**: Every obligation started during the run must have
///   reached a target at or after it started, wherever the actor ended.
///   (Weaker than the model check's whole walk, but catches stuck actors.)
fn check_liveness_post_simulation(
    model: &TemperModel,
    actor_states: &[(String, TemperModelState)],
    actor_reaches: &[Vec<bool>],
) -> Vec<LivenessViolation> {
    let mut violations = Vec::new();
    // Past a budget's deadline the model offers no actions, so every actor
    // would look deadlocked or stuck: the run proves nothing about liveness.
    if model.out_of_time() {
        return violations;
    }

    for ((actor_id, final_state), pending) in actor_states.iter().zip(actor_reaches) {
        for (rule, live) in model.liveness.iter().enumerate() {
            match &live.kind {
                LivenessKind::NoDeadlock { from } => {
                    if from.iter().any(|s| *s == final_state.status) {
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
                // Whether a record can still get somewhere needs every state
                // after it, not one run's last: the model check's walk has them.
                LivenessKind::CanReach { .. } => {}
                LivenessKind::ReachesState { targets, .. } => {
                    if pending[rule] {
                        violations.push(LivenessViolation {
                            actor_id: actor_id.clone(),
                            property: live.name.clone(),
                            description: format!(
                                "actor did not reach target states {:?} after the rule started; its run ended at '{}'",
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
    action: &TemperModelAction,
    state_before: &TemperModelState,
    state_after: &TemperModelState,
    tick: u64,
    violations: &mut Vec<InvariantViolation>,
) {
    // TypeInvariant: status must be in valid state set
    if !model.states.iter().any(|s| *s == state_after.status) {
        violations.push(InvariantViolation {
            actor_id: actor_id.to_string(),
            action: action.name.to_string(),
            state_before: state_before.clone(),
            state_after: state_after.clone(),
            invariant: "TypeInvariant: status not in valid states".to_string(),
            tick,
        });
    }

    let mut violated = |invariant: String| {
        violations.push(InvariantViolation {
            actor_id: actor_id.to_string(),
            action: action.name.to_string(),
            state_before: state_before.clone(),
            state_after: state_after.clone(),
            invariant,
            tick,
        });
    };

    // Check each resolved invariant from the spec
    for inv in &model.invariants {
        if truth(&inv.assert, &model.var_kinds, state_after) == Truth::False {
            violated(inv.name.clone());
        }
    }
    if let Some(rule) = crate::model::broken_by_action(model, action, state_before) {
        violated(rule.name.clone());
    }

    if model.terminal.iter().any(|s| *s == state_after.status) {
        let mut actions = Vec::new();
        model.actions(state_after, &mut actions);
        if !actions.is_empty() {
            violated(format!("Terminal({})", state_after.status));
        }
    }
}

#[cfg(test)]
mod tests;
