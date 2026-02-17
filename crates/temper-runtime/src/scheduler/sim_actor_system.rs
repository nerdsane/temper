//! Deterministic actor simulation system.
//!
//! [`SimActorSystem`] bridges [`SimScheduler`] and real actor handlers
//! ([`SimActorHandler`]). It runs real `TransitionTable::evaluate()` through
//! the scheduler with seed-controlled everything.
//!
//! Two modes:
//! - **Scripted**: call `step()` with specific (actor, action, params) tuples
//! - **Random**: call `run_random()` to explore randomly with fault injection
//!
//! Invariants are checked after every successful transition.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::clock::{LogicalClock, SimClock};
use super::context::{install_sim_context, SimContextGuard};
use super::id_gen::DeterministicIdGen;
use super::sim_handler::SimActorHandler;
use super::{DeterministicRng, FaultConfig, SimScheduler};

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
}

impl Default for SimActorSystemConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            max_ticks: 500,
            faults: FaultConfig::none(),
            max_actions_per_actor: 50,
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

/// Result of a simulation run.
#[derive(Debug, Clone)]
pub struct SimActorResult {
    /// Whether all invariants held.
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
    /// Final state per actor: (actor_id, status, item_count, event_count).
    pub actor_states: Vec<(String, String, usize, usize)>,
}

/// Invariant checker function signature.
pub type InvariantChecker = Box<dyn Fn(&str, &str, &str, usize) -> Option<String>>;

/// The deterministic actor simulation system.
///
/// Runs real [`SimActorHandler`] instances through [`SimScheduler`] with
/// full determinism: logical clock, deterministic UUIDs, seed-controlled
/// fault injection.
pub struct SimActorSystem {
    config: SimActorSystemConfig,
    actors: BTreeMap<String, Box<dyn SimActorHandler>>,
    action_counts: BTreeMap<String, usize>,
    scheduler: SimScheduler,
    clock: Arc<LogicalClock>,
    _id_gen: Arc<DeterministicIdGen>,
    _guard: SimContextGuard,
    rng: DeterministicRng,
    invariant_checker: Option<InvariantChecker>,
    violations: Vec<ActorInvariantViolation>,
    total_transitions: u64,
    total_messages: u64,
}

impl SimActorSystem {
    /// Create a new simulation system with the given config.
    pub fn new(config: SimActorSystemConfig) -> Self {
        let clock = Arc::new(LogicalClock::new());
        let id_gen = Arc::new(DeterministicIdGen::new(config.seed));
        let guard = install_sim_context(clock.clone(), id_gen.clone());
        let scheduler = SimScheduler::new(config.seed, config.faults.clone());
        let rng = DeterministicRng::new(config.seed.wrapping_add(7));

        Self {
            config,
            actors: BTreeMap::new(),
            action_counts: BTreeMap::new(),
            scheduler,
            clock,
            _id_gen: id_gen,
            _guard: guard,
            rng,
            invariant_checker: None,
            violations: Vec::new(),
            total_transitions: 0,
            total_messages: 0,
        }
    }

    /// Register an actor handler.
    pub fn register_actor(&mut self, id: &str, mut handler: Box<dyn SimActorHandler>) {
        self.scheduler.register_actor(id);
        handler.init().expect("actor init should succeed");
        self.actors.insert(id.to_string(), handler);
        self.action_counts.insert(id.to_string(), 0);
    }

    /// Set a custom invariant checker.
    ///
    /// The checker receives (actor_id, action, status, item_count) and returns
    /// `Some(description)` if an invariant is violated.
    pub fn set_invariant_checker(&mut self, checker: InvariantChecker) {
        self.invariant_checker = Some(checker);
    }

    // ===================================================================
    // Scripted Mode
    // ===================================================================

    /// Execute a specific action on a specific actor.
    ///
    /// Returns the actor's state as JSON on success, or an error string.
    pub fn step(
        &mut self,
        actor_id: &str,
        action: &str,
        params: &str,
    ) -> Result<serde_json::Value, String> {
        let handler = self
            .actors
            .get_mut(actor_id)
            .ok_or_else(|| format!("Unknown actor: {actor_id}"))?;

        let status_before = handler.current_status();
        self.clock.advance();
        self.total_messages += 1;

        let result = handler.handle_message(action, params);

        match &result {
            Ok(_) => {
                let status_after = handler.current_status();
                let item_count = handler.current_item_count();

                // Only count as transition if status or items actually changed
                let count = self.action_counts.get_mut(actor_id).unwrap(); // ci-ok: actor always in action_counts
                *count += 1;
                self.total_transitions += 1;

                // Check invariants
                self.check_invariants(
                    actor_id,
                    action,
                    &status_before,
                    &status_after,
                    item_count,
                    self.clock.tick(),
                );
            }
            Err(_) => {
                // Failed action — invariants should still hold on unchanged state
            }
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

    // ===================================================================
    // Random Mode
    // ===================================================================

    /// Run random exploration with fault injection.
    ///
    /// The RNG picks actors and actions. The scheduler delays/drops/crashes.
    /// Invariants are checked after every successful transition.
    pub fn run_random(&mut self) -> SimActorResult {
        for _tick in 0..self.config.max_ticks {
            if self.actors.is_empty() {
                break;
            }

            // Pick a random actor
            let actor_ids: Vec<String> = self.actors.keys().cloned().collect();
            let actor_idx = self.rng.next_bound(actor_ids.len());
            let actor_id = actor_ids[actor_idx].clone();

            // Check action budget
            let count = self.action_counts.get(&actor_id).copied().unwrap_or(0);
            if count >= self.config.max_actions_per_actor {
                continue;
            }

            // Get valid actions
            let valid = {
                let handler = self.actors.get(&actor_id).unwrap(); // ci-ok: actor_id from self.actors.keys()
                handler.valid_actions()
            };

            if valid.is_empty() {
                continue; // Terminal state
            }

            // Pick a random valid action
            let action_idx = self.rng.next_bound(valid.len());
            let action = valid[action_idx].clone();

            // Execute through the scheduler for fault injection
            self.scheduler.send(
                "sim-driver",
                &actor_id,
                &action,
                "{}",
            );
            self.total_messages += 1;

            let delivered = self.scheduler.tick();
            self.clock.advance();

            // Process delivered messages
            for msg in &delivered {
                if let Some(handler) = self.actors.get_mut(&msg.to) {
                    let status_before = handler.current_status();

                    match handler.handle_message(&msg.msg_type, &msg.payload) {
                        Ok(_) => {
                            let status_after = handler.current_status();
                            let item_count = handler.current_item_count();
                            *self.action_counts.get_mut(&msg.to).unwrap() += 1; // ci-ok: actor always in action_counts
                            self.total_transitions += 1;

                            self.check_invariants(
                                &msg.to,
                                &msg.msg_type,
                                &status_before,
                                &status_after,
                                item_count,
                                self.clock.tick(),
                            );
                        }
                        Err(_) => {
                            // Action failed — expected for invalid transitions
                        }
                    }
                }
            }

            // Drain any remaining scheduled messages
            self.scheduler.tick();
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
            all_invariants_held: self.violations.is_empty(),
            seed: self.config.seed,
            transitions: self.total_transitions,
            messages: self.total_messages,
            dropped: self.scheduler.total_dropped() as u64,
            violations: self.violations.clone(),
            actor_states,
        }
    }

    // ===================================================================
    // Invariant checking
    // ===================================================================

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
                let triggered = inv.when.is_empty()
                    || inv.when.iter().any(|s| s == status_after);
                if !triggered {
                    continue;
                }

                let violated = match &inv.assert {
                    super::sim_handler::SpecAssert::CounterPositive { var } => {
                        // Currently only "items" counter is tracked.
                        if var == "items" {
                            item_count == 0
                        } else {
                            false
                        }
                    }
                    super::sim_handler::SpecAssert::NoFurtherTransitions => {
                        // This is called after a successful transition. If
                        // status_before was the terminal state (one of the
                        // `when` states), a transition should not have fired.
                        inv.when.iter().any(|s| s == status_before)
                    }
                };

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
        if let Some(ref checker) = self.invariant_checker {
            if let Some(desc) = checker(actor_id, action, status_after, item_count) {
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
}
