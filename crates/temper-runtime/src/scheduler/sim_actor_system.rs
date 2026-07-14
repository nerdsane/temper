//! Deterministic actor simulation through [`SimScheduler`] and real
//! [`SimActorHandler`] implementations. Scripted mode uses [`SimActorSystem::step`];
//! random mode uses [`SimActorSystem::run_random`] with fault injection.
//! Invariants are checked after each successful transition.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use super::clock::{LogicalClock, SimClock};
use super::context::{SimContextGuard, install_sim_context};
use super::id_gen::DeterministicIdGen;
use super::sim_handler::SimActorHandler;
use super::{DeterministicRng, FaultConfig, SimScheduler};

mod invariant_eval;
use invariant_eval::evaluate_spec_assert;
mod callbacks;
mod random_budget;
use random_budget::{release, reserve};
mod recording;
pub use callbacks::{SimExecutionError, SimIntegrationResponses};

/// Configuration for a [`SimActorSystem`] run.
#[derive(Debug, Clone)]
pub struct SimActorSystemConfig {
    /// Seed for all non-determinism.
    pub seed: u64,
    /// Maximum ticks for random mode.
    pub max_ticks: u64,
    /// Fault injection configuration.
    pub faults: FaultConfig,
    /// Maximum actions per actor in random mode.
    pub max_actions_per_actor: usize,
    /// Maximum ready messages transferred in one bounded drain batch.
    pub message_batch_budget: usize,
    /// Maximum integration callbacks executed in one deterministic cascade.
    pub reaction_budget_per_tick: usize,
}

impl Default for SimActorSystemConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            max_ticks: 500,
            faults: FaultConfig::light(),
            max_actions_per_actor: 50,
            message_batch_budget: 1_024,
            reaction_budget_per_tick: 1_024,
        }
    }
}

/// An invariant violation found during actor simulation.
#[derive(Debug, Clone)]
pub struct ActorInvariantViolation {
    /// Which actor.
    pub actor_id: String,
    /// What action triggered it.
    pub action: String,
    /// Status before the action.
    pub status_before: String,
    /// Status after the action.
    pub status_after: String,
    /// Description of the violation.
    pub description: String,
    /// At what tick.
    pub tick: u64,
}

/// Complete transition, event, invariant, and final-state recording for
/// byte-exact comparison of runs using the same seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    /// Seed used.
    pub seed: u64,
    /// Every state transition that occurred: (tick, actor_id, action, from_status, to_status).
    pub transitions: Vec<(u64, String, String, String, String)>,
    /// Every event recorded by each actor (actor_id -> [event JSON strings]).
    pub events: BTreeMap<String, Vec<String>>,
    /// Final states: (actor_id, status, item_count, event_count, counters_json).
    pub final_states: Vec<(String, String, usize, usize, String)>,
    /// All invariant check results: (actor_id, invariant_name, passed).
    pub invariant_results: Vec<(String, String, bool)>,
}

/// Result of a simulation run.
#[derive(Debug, Clone)]
pub struct SimActorResult {
    /// Whether all invariants held and the simulation driver completed cleanly.
    pub all_invariants_held: bool,
    /// Seed used (for replay).
    pub seed: u64,
    /// Total successful transitions.
    pub transitions: u64,
    /// Total messages sent.
    pub messages: u64,
    /// Total messages dropped.
    pub dropped: u64,
    /// Invariant violations found.
    pub violations: Vec<ActorInvariantViolation>,
    /// Callback or driver failures that invalidate the run.
    pub execution_errors: Vec<SimExecutionError>,
    /// Final state per actor: (actor_id, status, item_count, event_count).
    pub actor_states: Vec<(String, String, usize, usize)>,
}

/// Invariant checker function signature.
pub type InvariantChecker = Box<dyn Fn(&str, &str, &str, usize) -> Option<String>>;

/// Runs real handlers through a logical clock, deterministic UUIDs, and
/// seed-controlled scheduler fault injection.
pub struct SimActorSystem {
    config: SimActorSystemConfig,
    actors: BTreeMap<String, Box<dyn SimActorHandler>>,
    action_counts: BTreeMap<String, usize>,
    random_in_flight_actions: BTreeMap<String, usize>,
    observed_scheduler_drops: usize,
    scheduler: SimScheduler,
    clock: Arc<LogicalClock>,
    _id_gen: Arc<DeterministicIdGen>,
    _guard: SimContextGuard,
    rng: DeterministicRng,
    invariant_checker: Option<InvariantChecker>,
    violations: Vec<ActorInvariantViolation>,
    execution_errors: Vec<SimExecutionError>,
    total_transitions: u64,
    total_messages: u64,
    /// Recorded transitions for RunRecord: (tick, actor_id, action, from_status, to_status).
    recorded_transitions: Vec<(u64, String, String, String, String)>,
    /// Recorded invariant results for RunRecord: (actor_id, invariant_name, passed).
    recorded_invariants: Vec<(String, String, bool)>,
    /// Integration callback configuration for WASM trigger simulation.
    integration_responses: SimIntegrationResponses,
    /// Pending integration callbacks to deliver: (actor_id, callback_action).
    pending_integration_callbacks: VecDeque<(String, String)>,
}

impl SimActorSystem {
    /// Create a new simulation system with the given config.
    pub fn new(config: SimActorSystemConfig) -> Self {
        assert!(
            config.message_batch_budget > 0,
            "message batch budget must be positive"
        );
        assert!(
            config.reaction_budget_per_tick > 0,
            "reaction budget per tick must be positive"
        );
        let clock = Arc::new(LogicalClock::new());
        let id_gen = Arc::new(DeterministicIdGen::new(config.seed));
        let guard = install_sim_context(clock.clone(), id_gen.clone());
        let mailbox_budget = usize::try_from(config.max_ticks)
            .expect("maximum ticks must fit the platform address space")
            .max(1);
        let scheduler =
            SimScheduler::with_mailbox_budget(config.seed, config.faults.clone(), mailbox_budget);
        let rng = DeterministicRng::new(config.seed.wrapping_add(7));

        Self {
            config,
            actors: BTreeMap::new(),
            action_counts: BTreeMap::new(),
            random_in_flight_actions: BTreeMap::new(),
            observed_scheduler_drops: 0,
            scheduler,
            clock,
            _id_gen: id_gen,
            _guard: guard,
            rng,
            invariant_checker: None,
            violations: Vec::new(),
            execution_errors: Vec::new(),
            total_transitions: 0,
            total_messages: 0,
            recorded_transitions: Vec::new(),
            recorded_invariants: Vec::new(),
            integration_responses: SimIntegrationResponses::new(),
            pending_integration_callbacks: VecDeque::new(),
        }
    }

    /// Register an actor handler.
    pub fn register_actor(&mut self, id: &str, mut handler: Box<dyn SimActorHandler>) {
        self.scheduler.register_actor(id);
        handler.init().expect("actor init should succeed");
        self.actors.insert(id.to_string(), handler);
        self.action_counts.insert(id.to_string(), 0);
        self.random_in_flight_actions.insert(id.to_string(), 0);
    }

    /// Set a custom invariant checker.
    ///
    /// The checker receives (actor_id, action, status, item_count) and returns
    /// `Some(description)` if an invariant is violated.
    pub fn set_invariant_checker(&mut self, checker: InvariantChecker) {
        self.invariant_checker = Some(checker);
    }

    /// Configure integration callback responses for WASM trigger simulation.
    ///
    /// When an actor emits a custom effect (trigger), the simulation system
    /// looks up the configured callback and auto-schedules it on the next tick.
    /// This lets DST explore both success and failure paths without executing
    /// real WASM modules.
    pub fn set_integration_responses(&mut self, responses: SimIntegrationResponses) {
        self.integration_responses = responses;
    }

    /// Execute a specific action on a specific actor.
    ///
    /// Returns the actor's state as JSON when both the primary action and its
    /// callback cascade succeed. If a callback fails, the primary action has
    /// already committed and this returns an error describing the callback;
    /// callers must not retry the primary action as though it were rolled back.
    pub fn step(
        &mut self,
        actor_id: &str,
        action: &str,
        params: &str,
    ) -> Result<serde_json::Value, String> {
        self.clock.advance();
        self.total_messages += 1;
        let result = self.apply_action(actor_id, action, params)?;
        self.deliver_integration_callbacks(&mut 0)?;
        Ok(result)
    }

    /// Apply one actor action without recursively delivering its callbacks.
    fn apply_action(
        &mut self,
        actor_id: &str,
        action: &str,
        params: &str,
    ) -> Result<serde_json::Value, String> {
        let (status_before, result, status_after, item_count) = {
            let handler = self
                .actors
                .get_mut(actor_id)
                .ok_or_else(|| format!("Unknown actor: {actor_id}"))?;

            let status_before = handler.current_status();
            let result = handler.handle_message(action, params);
            let status_after = handler.current_status();
            let item_count = handler.current_item_count();
            (status_before, result, status_after, item_count)
        };

        if result.is_ok() {
            let tick = self.clock.tick();

            let count = self
                .action_counts
                .get_mut(actor_id)
                .expect("registered actor");
            *count += 1;
            self.total_transitions += 1;

            self.recorded_transitions.push((
                tick,
                actor_id.to_string(),
                action.to_string(),
                status_before.clone(),
                status_after.clone(),
            ));

            self.check_invariants(
                actor_id,
                action,
                &status_before,
                &status_after,
                item_count,
                tick,
            );

            self.schedule_integration_callbacks(actor_id);
        }

        result
    }

    /// Assert that an actor is in the expected status.
    pub fn assert_status(&self, actor_id: &str, expected: &str) {
        let handler = self.actors.get(actor_id).unwrap_or_else(|| {
            panic!("Unknown actor: {actor_id}");
        });
        let actual = handler.current_status();
        assert_eq!(
            actual, expected,
            "Actor '{actor_id}' expected status '{expected}', got '{actual}'"
        );
    }

    /// Assert that an actor has the expected item count.
    pub fn assert_item_count(&self, actor_id: &str, expected: usize) {
        let handler = self.actors.get(actor_id).unwrap_or_else(|| {
            panic!("Unknown actor: {actor_id}");
        });
        let actual = handler.current_item_count();
        assert_eq!(
            actual, expected,
            "Actor '{actor_id}' expected {expected} items, got {actual}"
        );
    }

    /// Assert that an actor has the expected event count.
    pub fn assert_event_count(&self, actor_id: &str, expected: usize) {
        let handler = self.actors.get(actor_id).unwrap_or_else(|| {
            panic!("Unknown actor: {actor_id}");
        });
        let actual = handler.event_count();
        assert_eq!(
            actual, expected,
            "Actor '{actor_id}' expected {expected} events, got {actual}"
        );
    }

    /// Get an actor's events as JSON.
    pub fn events_json(&self, actor_id: &str) -> serde_json::Value {
        self.actors
            .get(actor_id)
            .map(|h| h.events_json())
            .unwrap_or(serde_json::Value::Null)
    }

    /// Get an actor's current status.
    pub fn status(&self, actor_id: &str) -> String {
        self.actors
            .get(actor_id)
            .map(|h| h.current_status())
            .unwrap_or_default()
    }

    /// Whether there are any violations.
    pub fn has_violations(&self) -> bool {
        !self.violations.is_empty()
    }

    /// Get collected violations.
    pub fn violations(&self) -> &[ActorInvariantViolation] {
        &self.violations
    }

    /// Get callback and driver failures collected during random simulation.
    pub fn execution_errors(&self) -> &[SimExecutionError] {
        &self.execution_errors
    }

    /// Run random exploration with fault injection.
    ///
    /// The RNG picks actors and actions. The scheduler delays/drops/crashes.
    /// Invariants are checked after every successful transition.
    pub fn run_random(&mut self) -> SimActorResult {
        'simulation: for _tick in 0..self.config.max_ticks {
            if self.actors.is_empty() {
                break;
            }

            // Pick a random actor
            let actor_ids: Vec<String> = self.actors.keys().cloned().collect();
            let actor_idx = self.rng.next_bound(actor_ids.len());
            let actor_id = actor_ids[actor_idx].clone();

            // Check action budget
            let completed = self.action_counts.get(&actor_id).copied().unwrap_or(0);
            let in_flight = self
                .random_in_flight_actions
                .get(&actor_id)
                .copied()
                .unwrap_or(0);
            if completed + in_flight < self.config.max_actions_per_actor {
                let valid = {
                    let handler = self.actors.get(&actor_id).expect("selected actor");
                    handler.valid_actions()
                };

                if !valid.is_empty() {
                    let action_idx = self.rng.next_bound(valid.len());
                    let action = valid[action_idx].clone();
                    self.scheduler.send("sim-driver", &actor_id, &action, "{}");
                    reserve(&mut self.random_in_flight_actions, &actor_id);
                    self.total_messages += 1;
                }
            }

            // Logical time and ready delivery progress independently of whether
            // a new action was eligible this tick.
            self.scheduler.tick();
            self.clock.advance();
            for dropped in &self.scheduler.dropped_log()[self.observed_scheduler_drops..] {
                if dropped.from == "sim-driver" {
                    release(&mut self.random_in_flight_actions, &dropped.to, "dropped");
                }
            }
            self.observed_scheduler_drops = self.scheduler.dropped_log().len();
            let mut reactions = 0;
            loop {
                let delivered = self.scheduler.drain_ready(self.config.message_batch_budget);
                for msg in &delivered {
                    release(&mut self.random_in_flight_actions, &msg.to, "delivered");
                    let _ = self.apply_action(&msg.to, &msg.msg_type, &msg.payload);
                }
                if self.deliver_integration_callbacks(&mut reactions).is_err() {
                    break 'simulation;
                }
                if _tick + 1 < self.config.max_ticks || !self.scheduler.has_ready_messages() {
                    break;
                }
            }
        }

        let actor_states: Vec<_> = self
            .actors
            .iter()
            .map(|(id, h)| {
                (
                    id.clone(),
                    h.current_status(),
                    h.current_item_count(),
                    h.event_count(),
                )
            })
            .collect();

        SimActorResult {
            all_invariants_held: self.violations.is_empty() && self.execution_errors.is_empty(),
            seed: self.config.seed,
            transitions: self.total_transitions,
            messages: self.total_messages,
            dropped: self.scheduler.total_dropped() as u64,
            violations: self.violations.clone(),
            execution_errors: self.execution_errors.clone(),
            actor_states,
        }
    }

    fn check_invariants(
        &mut self,
        actor_id: &str,
        action: &str,
        status_before: &str,
        status_after: &str,
        item_count: usize,
        tick: u64,
    ) {
        // 1. Check spec-derived invariants from the handler (automatic).
        if let Some(handler) = self.actors.get(actor_id) {
            let invariants: Vec<_> = handler.spec_invariants().to_vec();
            for inv in &invariants {
                let triggered = inv.when.is_empty() || inv.when.iter().any(|s| s == status_after);
                if !triggered {
                    continue;
                }

                let passed = evaluate_spec_assert(
                    &inv.assert,
                    handler.as_ref(),
                    &inv.when,
                    status_before,
                    status_after,
                    item_count,
                );
                let violated = !passed;

                self.recorded_invariants
                    .push((actor_id.to_string(), inv.name.clone(), !violated));

                if violated {
                    self.violations.push(ActorInvariantViolation {
                        actor_id: actor_id.to_string(),
                        action: action.to_string(),
                        status_before: status_before.to_string(),
                        status_after: status_after.to_string(),
                        description: format!("{}: violated after '{}'", inv.name, action),
                        tick,
                    });
                }
            }
        }

        // 2. Check manual invariant checker (backward-compatible).
        if let Some(ref checker) = self.invariant_checker
            && let Some(desc) = checker(actor_id, action, status_after, item_count)
        {
            self.violations.push(ActorInvariantViolation {
                actor_id: actor_id.to_string(),
                action: action.to_string(),
                status_before: status_before.to_string(),
                status_after: status_after.to_string(),
                description: desc,
                tick,
            });
        }
    }
}

#[cfg(test)]
#[path = "sim_actor_system/tests.rs"]
mod tests;
