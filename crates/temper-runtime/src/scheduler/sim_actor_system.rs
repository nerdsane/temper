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
    /// Pending integration callbacks to deliver: (actor_id, callback_action).
    pending_integration_callbacks: Vec<(String, String)>,
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
            pending_integration_callbacks: Vec::new(),
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

        let result = handler.handle_message(action, params);

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

        // Deliver any pending integration callbacks
        if !self.pending_integration_callbacks.is_empty() {
            self.deliver_integration_callbacks();
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
            self.scheduler.send("sim-driver", &actor_id, &action, "{}");
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
                            let tick = self.clock.tick();
                            *self.action_counts.get_mut(&msg.to).unwrap() += 1; // ci-ok: actor always in action_counts
                            self.total_transitions += 1;

                            // Record the transition
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

                            // Schedule integration callbacks for any custom effects
                            self.schedule_integration_callbacks(&msg.to);
                        }
                        Err(_) => {
                            // Action failed — expected for invalid transitions
                        }
                    }
                }
            }

            // Deliver any pending integration callbacks
            if !self.pending_integration_callbacks.is_empty() {
                self.deliver_integration_callbacks();
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

        // Derive entity_type from actor_id (convention: "EntityType:EntityId" or just id)
        // For simplicity, check against all registered entity_type patterns.
        for trigger in &callbacks {
            // Try matching with the actor_id as-is for the entity_type lookup
            if let Some(callback_action) =
                self.integration_responses.get_callback(actor_id, trigger)
            {
                self.pending_integration_callbacks
                    .push((actor_id.to_string(), callback_action.to_string()));
            }
            // Also try splitting on ':' (e.g., "Order:o1" → entity_type = "Order")
            else if let Some(colon_pos) = actor_id.find(':') {
                let entity_type = &actor_id[..colon_pos];
                if let Some(callback_action) = self
                    .integration_responses
                    .get_callback(entity_type, trigger)
                {
                    self.pending_integration_callbacks
                        .push((actor_id.to_string(), callback_action.to_string()));
                }
            }
        }
    }

    /// Deliver any pending integration callbacks by executing them as actions.
    fn deliver_integration_callbacks(&mut self) {
        let callbacks: Vec<(String, String)> =
            self.pending_integration_callbacks.drain(..).collect();
        for (actor_id, callback_action) in callbacks {
            // Execute the callback as a regular step (this checks invariants too)
            let _ = self.step(&actor_id, &callback_action, "{}");
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

/// Evaluate a [`SpecAssert`] against handler state. Returns `true` if the
/// assertion holds, `false` if violated. Recurses through `And`/`Or`.
fn evaluate_spec_assert(
    assert: &super::sim_handler::SpecAssert,
    handler: &dyn super::sim_handler::SimActorHandler,
    when: &[String],
    status_before: &str,
    status_after: &str,
    item_count: usize,
) -> bool {
    use super::sim_handler::{CompareOp, SpecAssert};

    match assert {
        SpecAssert::CounterPositive { var } => {
            if var == "items" {
                item_count > 0
            } else {
                true // Unknown counter: not in scope for invariant checking here.
            }
        }
        SpecAssert::NoFurtherTransitions => {
            // Holds unless status_before was a terminal state in `when`.
            !when.iter().any(|s| s == status_before)
        }
        SpecAssert::OrderingConstraint { before, after } => {
            if status_after == after.as_str() {
                let events = handler.events_json();
                if let Some(arr) = events.as_array() {
                    arr.iter().any(|e| {
                        e.get("to_status").and_then(|s| s.as_str()) == Some(before.as_str())
                    })
                } else {
                    true
                }
            } else {
                true
            }
        }
        SpecAssert::NeverState { state } => status_after != state.as_str(),
        SpecAssert::CounterCompare { var, op, value } => {
            let counter_val = if var == "items" { item_count } else { 0 };
            match op {
                CompareOp::Gt => counter_val > *value,
                CompareOp::Gte => counter_val >= *value,
                CompareOp::Lt => counter_val < *value,
                CompareOp::Lte => counter_val <= *value,
                CompareOp::Eq => counter_val == *value,
            }
        }
        SpecAssert::BoolRequired { var, expect } => {
            handler.bool_field(var).unwrap_or(false) == *expect
        }
        SpecAssert::And(parts) => parts.iter().all(|p| {
            evaluate_spec_assert(p, handler, when, status_before, status_after, item_count)
        }),
        SpecAssert::Or(parts) => parts.iter().any(|p| {
            evaluate_spec_assert(p, handler, when, status_before, status_after, item_count)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ARN-236: delayed-message ownership properties ─────────────────────
    //
    // Scheduler::tick() both enqueues a due message into the target mailbox
    // AND returns a clone; the drivers process the returned clones and never
    // drain mailboxes, and each loop iteration ends with a bare tick() whose
    // returned deliveries are discarded. Consequences these tests pin:
    // processed messages remain queued forever, deliveries surfaced only by
    // the trailing tick are never applied, and a failing integration
    // callback still yields a green run.

    /// Accepts every action, counts applications, and (optionally) emits a
    /// callback trigger whose configured action always fails.
    struct CountingHandler {
        applications: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        emit_trigger: bool,
        fired: bool,
    }

    impl SimActorHandler for CountingHandler {
        fn init(&mut self) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({"status": "Ready"}))
        }
        fn handle_message(
            &mut self,
            action: &str,
            _params: &str,
        ) -> Result<serde_json::Value, String> {
            if action == "AlwaysFails" {
                return Err("callback action rejected".to_string());
            }
            self.applications
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.emit_trigger {
                self.fired = true;
            }
            Ok(serde_json::json!({"status": "Ready"}))
        }
        fn current_status(&self) -> String {
            "Ready".to_string()
        }
        fn current_item_count(&self) -> usize {
            0
        }
        fn event_count(&self) -> usize {
            self.applications.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn valid_actions(&self) -> Vec<String> {
            vec!["Step".to_string()]
        }
        fn events_json(&self) -> serde_json::Value {
            serde_json::json!([])
        }
        fn pending_callbacks(&self) -> Vec<String> {
            if self.fired {
                vec!["boom_trigger".to_string()]
            } else {
                Vec::new()
            }
        }
    }

    fn counting_system(
        seed: u64,
        faults: FaultConfig,
    ) -> (
        SimActorSystem,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let applications = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let config = SimActorSystemConfig {
            seed,
            max_ticks: 200,
            faults,
            max_actions_per_actor: 30,
        };
        let mut system = SimActorSystem::new(config);
        system.register_actor(
            "counter",
            Box::new(CountingHandler {
                applications: applications.clone(),
                emit_trigger: false,
                fired: false,
            }),
        );
        (system, applications)
    }

    /// No processed message may remain queued: after a run, every mailbox is
    /// empty and the scheduler is quiescent.
    #[test]
    fn arn236_no_processed_message_remains_queued() {
        let faults = FaultConfig {
            message_delay_prob: 0.0,
            max_delay_ticks: 0,
            message_drop_prob: 0.0,
            actor_crash_prob: 0.0,
            actor_restart_prob: 0.0,
        };
        let (mut system, _applications) = counting_system(7, faults);
        let result = system.run_random();
        assert!(result.messages > 0, "the run must exercise messages");

        assert_eq!(
            system.scheduler.mailbox_depth("counter"),
            0,
            "a processed message must not remain queued in its mailbox \
             (single-ownership: applied messages are consumed, not cloned)"
        );
        assert!(
            system.scheduler.is_quiescent(),
            "after a fault-free run every delivered message must be consumed"
        );
    }

    /// Every scheduled message is applied exactly once — deliveries surfaced
    /// by the loop's trailing tick must not be silently discarded. With
    /// message delays (no drops, no crashes), every sent message is
    /// eventually due, so applications must equal sends across all seeds.
    #[test]
    fn arn236_every_scheduled_message_is_applied_exactly_once() {
        for seed in 0..50u64 {
            let faults = FaultConfig {
                message_delay_prob: 0.5,
                max_delay_ticks: 8,
                message_drop_prob: 0.0,
                actor_crash_prob: 0.0,
                actor_restart_prob: 0.0,
            };
            let (mut system, applications) = counting_system(seed, faults);
            let result = system.run_random();
            let applied = applications.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                applied as u64, result.messages,
                "seed {seed}: every scheduled message must be applied exactly \
                 once ({} sent, {applied} applied) — a delivery surfaced only \
                 by the trailing tick must not be discarded, and none may \
                 apply twice",
                result.messages
            );
        }
    }

    /// A failing integration callback must fail the run, not vanish.
    #[test]
    fn arn236_callback_failure_is_part_of_the_simulation_result() {
        let applications = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let config = SimActorSystemConfig {
            seed: 11,
            max_ticks: 50,
            faults: FaultConfig {
                message_delay_prob: 0.0,
                max_delay_ticks: 0,
                message_drop_prob: 0.0,
                actor_crash_prob: 0.0,
                actor_restart_prob: 0.0,
            },
            max_actions_per_actor: 3,
        };
        let mut system = SimActorSystem::new(config);
        system.set_integration_responses(SimIntegrationResponses::new().on_trigger(
            "counter",
            "boom_trigger",
            "AlwaysFails",
        ));
        system.register_actor(
            "counter",
            Box::new(CountingHandler {
                applications: applications.clone(),
                emit_trigger: true,
                fired: false,
            }),
        );

        let result = system.run_random();
        assert!(
            !result.all_invariants_held || !result.violations.is_empty(),
            "a rejected integration callback must surface in the simulation \
             result — a green run that silently discarded a callback failure \
             does not faithfully exercise the schedule"
        );
    }

    #[test]
    fn integration_responses_empty_returns_none() {
        let responses = SimIntegrationResponses::new();
        assert!(responses.get_callback("Order", "payment_trigger").is_none());
    }

    #[test]
    fn integration_responses_on_trigger_and_get_callback() {
        let responses = SimIntegrationResponses::new()
            .on_trigger("Order", "payment_trigger", "ConfirmPayment")
            .on_trigger("Invoice", "send_trigger", "MarkSent");

        assert_eq!(
            responses.get_callback("Order", "payment_trigger"),
            Some("ConfirmPayment")
        );
        assert_eq!(
            responses.get_callback("Invoice", "send_trigger"),
            Some("MarkSent")
        );
        assert!(responses.get_callback("Order", "send_trigger").is_none());
        assert!(
            responses
                .get_callback("Unknown", "payment_trigger")
                .is_none()
        );
    }

    #[test]
    fn integration_responses_overwrite() {
        let responses = SimIntegrationResponses::new()
            .on_trigger("Order", "trigger", "ActionA")
            .on_trigger("Order", "trigger", "ActionB");

        assert_eq!(responses.get_callback("Order", "trigger"), Some("ActionB"));
    }

    #[test]
    fn config_default_values() {
        let config = SimActorSystemConfig::default();
        assert_eq!(config.seed, 42);
        assert_eq!(config.max_ticks, 500);
        assert_eq!(config.max_actions_per_actor, 50);
    }

    #[test]
    fn run_record_equality() {
        let r1 = RunRecord {
            seed: 42,
            transitions: vec![(
                1,
                "a".into(),
                "Submit".into(),
                "Draft".into(),
                "Submitted".into(),
            )],
            events: BTreeMap::new(),
            final_states: vec![],
            invariant_results: vec![],
        };
        let r2 = r1.clone();
        assert_eq!(r1, r2);
    }

    #[test]
    fn run_record_inequality_on_seed() {
        let r1 = RunRecord {
            seed: 42,
            transitions: vec![],
            events: BTreeMap::new(),
            final_states: vec![],
            invariant_results: vec![],
        };
        let r2 = RunRecord {
            seed: 99,
            ..r1.clone()
        };
        assert_ne!(r1, r2);
    }
}
