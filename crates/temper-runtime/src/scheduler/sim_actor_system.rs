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
use super::context::{SimContextGuard, install_sim_context};
use super::id_gen::DeterministicIdGen;
use super::sim_handler::SimActorHandler;
use super::{DeterministicRng, FaultConfig, SimScheduler};

/// Configures how integration callbacks are delivered in simulation.
///
/// Maps `(entity_type, trigger_name)` → callback action name. When a simulated
/// entity emits a custom effect matching a trigger, the system auto-schedules
/// the configured callback action on the next tick. This lets DST explore both
/// success and failure paths without executing real WASM modules.
#[derive(Debug, Clone, Default)]
pub struct SimIntegrationResponses {
    /// Maps (entity_type, trigger_name) → callback action name.
    responses: BTreeMap<(String, String), String>,
}

impl SimIntegrationResponses {
    /// Create an empty integration response map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure a success callback for a trigger.
    pub fn on_trigger(mut self, entity_type: &str, trigger: &str, callback_action: &str) -> Self {
        self.responses.insert(
            (entity_type.to_string(), trigger.to_string()),
            callback_action.to_string(),
        );
        self
    }

    /// Look up the callback action for a trigger.
    pub fn get_callback(&self, entity_type: &str, trigger: &str) -> Option<&str> {
        self.responses
            .get(&(entity_type.to_string(), trigger.to_string()))
            .map(|s| s.as_str())
    }
}

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
            faults: FaultConfig::light(),
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

/// Complete recording of a simulation run for determinism comparison.
///
/// Captures every state transition, every event, and every final state so that
/// two runs with the same seed can be compared for byte-exact equality.
/// This is the FoundationDB principle: same seed MUST produce identical output.
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
    /// Recorded transitions for RunRecord: (tick, actor_id, action, from_status, to_status).
    recorded_transitions: Vec<(u64, String, String, String, String)>,
    /// Recorded invariant results for RunRecord: (actor_id, invariant_name, passed).
    recorded_invariants: Vec<(String, String, bool)>,
    /// Integration callback configuration for WASM trigger simulation.
    integration_responses: SimIntegrationResponses,
    /// Actor-local wall-clock offsets applied only while that actor handles a message.
    actor_clock_skew_ms: BTreeMap<String, i64>,
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
            recorded_transitions: Vec::new(),
            recorded_invariants: Vec::new(),
            integration_responses: SimIntegrationResponses::new(),
            actor_clock_skew_ms: BTreeMap::new(),
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

    /// Configure integration callback responses for WASM trigger simulation.
    ///
    /// When an actor emits a custom effect (trigger), the simulation system
    /// looks up the configured callback and auto-schedules it on the next tick.
    /// This lets DST explore both success and failure paths without executing
    /// real WASM modules.
    pub fn set_integration_responses(&mut self, responses: SimIntegrationResponses) {
        self.integration_responses = responses;
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

        let skew_ms = self.actor_clock_skew_ms.get(actor_id).copied().unwrap_or(0);
        self.clock.set_skew_ms(skew_ms);
        let result = handler.handle_message(action, params);
        self.clock.set_skew_ms(0);

        match &result {
            Ok(_) => {
                let status_after = handler.current_status();
                let item_count = handler.current_item_count();
                let tick = self.clock.tick();

                // Only count as transition if status or items actually changed
                let count = self.action_counts.get_mut(actor_id).unwrap(); // ci-ok: actor always in action_counts
                *count += 1;
                self.total_transitions += 1;

                // Record the transition
                self.recorded_transitions.push((
                    tick,
                    actor_id.to_string(),
                    action.to_string(),
                    status_before.clone(),
                    status_after.clone(),
                ));

                // Check invariants
                self.check_invariants(
                    actor_id,
                    action,
                    &status_before,
                    &status_after,
                    item_count,
                    tick,
                );

                // Schedule integration callbacks for any custom effects
                self.schedule_integration_callbacks(actor_id);
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

    /// Return the actor's committed event sequence.
    pub fn event_sequence(&self, actor_id: &str) -> u64 {
        self.actors
            .get(actor_id)
            .map(|handler| handler.event_sequence())
            .unwrap_or(0)
    }

    /// Return the number of messages dropped by deterministic fault injection.
    pub fn dropped_messages(&self) -> usize {
        self.scheduler.total_dropped()
    }

    /// Get an actor's current status.
    pub fn status(&self, actor_id: &str) -> String {
        self.actors
            .get(actor_id)
            .map(|h| h.current_status())
            .unwrap_or_default()
    }

    /// Configure a deterministic clock offset for one actor.
    pub fn set_actor_clock_skew_ms(&mut self, actor_id: &str, skew_ms: i64) {
        assert!(
            self.actors.contains_key(actor_id),
            "clock skew actor must exist"
        );
        self.actor_clock_skew_ms
            .insert(actor_id.to_string(), skew_ms);
    }

    /// Advance logical time by a deterministic forward jump.
    pub fn jump_clock_by(&self, ticks: u64) {
        assert!(ticks > 0, "clock jump must consume at least one tick");
        self.clock.advance_by(ticks);
    }

    /// Inject a crash/restart edge and reconstruct the actor immediately.
    pub fn crash_and_restart_actor(&mut self, actor_id: &str) {
        self.scheduler.crash_actor(actor_id);
        self.scheduler.restart_actor(actor_id);
        self.reconstruct_restarted_actors();
    }

    /// Queue an actor-to-actor message through the fault-injecting scheduler.
    pub fn send_actor_message(&mut self, from: &str, to: &str, action: &str, params: &str) {
        assert!(self.actors.contains_key(from), "source actor must exist");
        assert!(self.actors.contains_key(to), "target actor must exist");
        self.scheduler.send(from, to, action, params);
        self.total_messages = self.total_messages.saturating_add(1);
    }

    /// Run queued actor messages for a bounded number of ticks.
    pub fn run_queued(&mut self, tick_budget: u64) -> u64 {
        assert!(
            tick_budget > 0,
            "queued delivery requires a positive tick budget"
        );
        let mut consumed = 0;
        while consumed < tick_budget && !self.scheduler.is_quiescent() {
            self.tick_and_apply_ready();
            consumed += 1;
        }
        if !self.scheduler.is_quiescent() {
            self.record_delivery_violation(
                "sim-driver",
                "RunQueued",
                String::new(),
                format!("queued delivery did not quiesce within {tick_budget} ticks"),
            );
        }
        consumed
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

            // Preserve per-actor mailbox serialization: do not choose a new
            // action from stale state while an earlier action is in flight.
            if self.scheduler.has_in_flight(&actor_id) {
                self.tick_and_apply_ready();
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
            let params = self
                .actors
                .get(&actor_id)
                .map(|handler| handler.params_for_action(&action))
                .unwrap_or_else(|| "{}".to_string());

            // Execute through the scheduler for fault injection
            self.scheduler
                .send("sim-driver", &actor_id, &action, &params);
            self.total_messages += 1;

            self.tick_and_apply_ready();
        }

        // Delays can move a delivery past the final exploration iteration.
        // Flush through the same mailbox-consumption path under an explicit
        // budget instead of discarding a final tick's deliveries.
        let mut flush_ticks = 0;
        while flush_ticks < self.config.max_ticks && !self.scheduler.is_quiescent() {
            self.tick_and_apply_ready();
            flush_ticks += 1;
        }
        if !self.scheduler.is_quiescent() {
            self.record_delivery_violation(
                "sim-driver",
                "RunRandomFlush",
                String::new(),
                format!(
                    "random delivery did not quiesce within {} flush ticks",
                    self.config.max_ticks
                ),
            );
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

    /// Run random exploration and return a full [`RunRecord`] alongside the result.
    ///
    /// This is the recording variant of [`run_random()`]. The `RunRecord` captures
    /// every transition, every event, and every final state for determinism
    /// comparison. Two calls with the same seed MUST produce identical records.
    pub fn run_random_recorded(&mut self) -> (SimActorResult, RunRecord) {
        let result = self.run_random();

        // Collect events from each actor
        let events: BTreeMap<String, Vec<String>> = self
            .actors
            .iter()
            .map(|(id, handler)| {
                let events_val = handler.events_json();
                let event_strings = match events_val {
                    serde_json::Value::Array(arr) => arr
                        .iter()
                        .map(|v| serde_json::to_string(v).unwrap_or_default())
                        .collect(),
                    _ => Vec::new(),
                };
                (id.clone(), event_strings)
            })
            .collect();

        // Collect final states with counters serialized as JSON
        let final_states: Vec<_> = self
            .actors
            .iter()
            .map(|(id, handler)| {
                let status = handler.current_status();
                let item_count = handler.current_item_count();
                let event_count = handler.event_count();
                // Serialize the full events_json as a proxy for counters
                // since SimActorHandler doesn't expose counters directly.
                // The events contain all state change details.
                let counters_json =
                    serde_json::to_string(&handler.events_json()).unwrap_or_default();
                (id.clone(), status, item_count, event_count, counters_json)
            })
            .collect();

        let record = RunRecord {
            seed: self.config.seed,
            transitions: self.recorded_transitions.clone(),
            events,
            final_states,
            invariant_results: self.recorded_invariants.clone(),
        };

        (result, record)
    }

    // ===================================================================
    // Integration callback scheduling
    // ===================================================================

    /// Check for pending integration callbacks and schedule them.
    ///
    /// After a successful action, the handler may have emitted custom effects
    /// (integration triggers). This method looks up configured callbacks and
    /// queues them for delivery on the next tick.
    fn schedule_integration_callbacks(&mut self, actor_id: &str) {
        let handler = match self.actors.get(actor_id) {
            Some(h) => h,
            None => return,
        };

        let callbacks = handler.pending_callbacks();
        if callbacks.is_empty() {
            return;
        }

        // Resolve every configured callback before mutating the scheduler.
        let mut scheduled = Vec::new();
        for trigger in &callbacks {
            let callback = self
                .integration_responses
                .get_callback(actor_id, trigger)
                .or_else(|| {
                    let colon_pos = actor_id.find(':')?;
                    let entity_type = &actor_id[..colon_pos];
                    self.integration_responses
                        .get_callback(entity_type, trigger)
                });
            if let Some(callback_action) = callback {
                scheduled.push(callback_action.to_string());
            }
        }

        for callback_action in scheduled {
            self.scheduler
                .send("sim-integration", actor_id, &callback_action, "{}");
            self.total_messages = self.total_messages.saturating_add(1);
        }
    }

    /// Advance one scheduler tick, reconstruct restarts, drain mailboxes, and
    /// apply every ready message exactly once.
    fn tick_and_apply_ready(&mut self) {
        self.scheduler.tick();
        self.clock.advance();
        self.reconstruct_restarted_actors();
        let delivered = self.scheduler.drain_ready();
        self.process_delivered_messages(&delivered);
        self.scheduler.finish_tick();
    }

    fn reconstruct_restarted_actors(&mut self) {
        for actor_id in self.scheduler.take_restarted_actors() {
            let handler = self
                .actors
                .get_mut(&actor_id)
                .unwrap_or_else(|| panic!("restarted actor '{actor_id}' must be registered"));
            handler
                .restart()
                .unwrap_or_else(|error| panic!("actor '{actor_id}' restart failed: {error}"));
        }
    }

    fn process_delivered_messages(&mut self, delivered: &[super::SimMessage]) {
        for msg in delivered {
            let Some(handler) = self.actors.get_mut(&msg.to) else {
                self.record_delivery_violation(
                    &msg.to,
                    &msg.msg_type,
                    String::new(),
                    "scheduled message targeted an unknown actor".to_string(),
                );
                continue;
            };
            let status_before = handler.current_status();
            let skew_ms = self.actor_clock_skew_ms.get(&msg.to).copied().unwrap_or(0);
            self.clock.set_skew_ms(skew_ms);
            let outcome = handler.handle_message(&msg.msg_type, &msg.payload);
            self.clock.set_skew_ms(0);

            let Err(error) = outcome else {
                let status_after = handler.current_status();
                let item_count = handler.current_item_count();
                let tick = self.clock.tick();
                let action_count = self.action_counts.get_mut(&msg.to).unwrap_or_else(|| {
                    panic!("delivered actor '{}' must have an action count", msg.to)
                });
                *action_count += 1;
                self.total_transitions += 1;
                self.recorded_transitions.push((
                    tick,
                    msg.to.clone(),
                    msg.msg_type.clone(),
                    status_before.clone(),
                    status_after.clone(),
                ));
                self.check_invariants(
                    &msg.to,
                    &msg.msg_type,
                    &status_before,
                    &status_after,
                    item_count,
                    tick,
                );
                self.schedule_integration_callbacks(&msg.to);
                continue;
            };

            let description = if msg.from == "sim-integration" {
                format!("integration callback rejected: {error}")
            } else {
                format!("scheduled handler rejected message: {error}")
            };
            self.record_delivery_violation(
                &msg.to,
                &msg.msg_type,
                status_before.clone(),
                description,
            );
        }
    }

    fn record_delivery_violation(
        &mut self,
        actor_id: &str,
        action: &str,
        status: String,
        description: String,
    ) {
        self.violations.push(ActorInvariantViolation {
            actor_id: actor_id.to_string(),
            action: action.to_string(),
            status_before: status.clone(),
            status_after: status,
            description,
            tick: self.clock.tick(),
        });
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
                let triggered = inv.when.is_empty() || inv.when.iter().any(|s| s == status_after);
                if !triggered {
                    continue;
                }

                let passed = evaluate_spec_assert(
                    &inv.assert,
                    handler.as_ref(),
                    status_before,
                    status_after,
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

/// Evaluate a [`SpecAssert`] against handler state. Returns `true` if the
/// assertion holds, `false` if violated. Recurses through `And`/`Or`.
fn evaluate_spec_assert(
    assert: &super::sim_handler::SpecAssert,
    handler: &dyn super::sim_handler::SimActorHandler,
    status_before: &str,
    status_after: &str,
) -> bool {
    use super::sim_handler::{CompareOp, SpecAssert};

    match assert {
        SpecAssert::CounterPositive { var } => {
            handler.counter_value(var).is_some_and(|value| value > 0)
        }
        SpecAssert::NoFurtherTransitions => handler.valid_actions().is_empty(),
        SpecAssert::OrderingConstraint { before, after } => {
            if status_after == after.as_str() {
                if status_before == before.as_str() {
                    return true;
                }
                let events = handler.events_json();
                if let Some(arr) = events.as_array() {
                    arr.iter().any(|e| {
                        e.get("to_status").and_then(|s| s.as_str()) == Some(before.as_str())
                    })
                } else {
                    false
                }
            } else {
                true
            }
        }
        SpecAssert::NeverState { state } => status_after != state.as_str(),
        SpecAssert::CounterCompare { var, op, value } => {
            let Some(counter_val) = handler.counter_value(var) else {
                return false;
            };
            match op {
                CompareOp::Gt => counter_val > *value,
                CompareOp::Gte => counter_val >= *value,
                CompareOp::Lt => counter_val < *value,
                CompareOp::Lte => counter_val <= *value,
                CompareOp::Eq => counter_val == *value,
            }
        }
        SpecAssert::CounterCompareCounter { left, op, right } => {
            let (Some(left), Some(right)) =
                (handler.counter_value(left), handler.counter_value(right))
            else {
                return false;
            };
            match op {
                CompareOp::Gt => left > right,
                CompareOp::Gte => left >= right,
                CompareOp::Lt => left < right,
                CompareOp::Lte => left <= right,
                CompareOp::Eq => left == right,
            }
        }
        SpecAssert::BoolRequired { var, expect } => handler.bool_field(var) == Some(*expect),
        SpecAssert::StringNonEmpty { var } => handler
            .string_value(var)
            .is_some_and(|value| !value.is_empty()),
        SpecAssert::And(parts) => parts
            .iter()
            .all(|p| evaluate_spec_assert(p, handler, status_before, status_after)),
        SpecAssert::Or(parts) => parts
            .iter()
            .any(|p| evaluate_spec_assert(p, handler, status_before, status_after)),
    }
}

#[cfg(test)]
#[path = "test_sim_actor_system.rs"]
mod tests;
