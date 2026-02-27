//! Deterministic simulation scheduler and actor system.
//!
//! Provides a single-threaded, seed-controlled message delivery system
//! inspired by FoundationDB's simulation testing and TigerBeetle's VOPR.
//!
//! Key properties:
//! - **Deterministic**: given the same seed, message ordering is identical
//! - **Reproducible**: any failure can be replayed by re-using the seed
//! - **Fault injection**: messages can be delayed, reordered, dropped, or
//!   actors can be crashed — all controlled by the seed
//! - **Single-threaded**: no real concurrency, all non-determinism eliminated
//!
//! ## Modules
//!
//! - [`clock`]: Simulation-aware clock (`WallClock` / `LogicalClock`)
//! - [`id_gen`]: Simulation-aware UUID generator (`RealIdGen` / `DeterministicIdGen`)
//! - [`context`]: Thread-local `SimContext` with `sim_now()` and `sim_uuid()`
//! - [`sim_handler`]: Type-erased `SimActorHandler` trait
//! - [`sim_actor_system`]: `SimActorSystem` — runs real handlers through the scheduler

pub mod clock;
pub mod context;
pub mod id_gen;
pub mod sim_actor_system;
pub mod sim_handler;

// Re-export key types from submodules.
pub use clock::{LogicalClock, SimClock, WallClock};
pub use context::{
    SimContextGuard, install_deterministic_context, install_sim_context, sim_now, sim_uuid,
};
pub use id_gen::{DeterministicIdGen, RealIdGen, SimIdGen};
pub use sim_actor_system::{
    ActorInvariantViolation, RunRecord, SimActorResult, SimActorSystem, SimActorSystemConfig,
    SimIntegrationResponses,
};
pub use sim_handler::{CompareOp, SimActorHandler, SpecAssert, SpecInvariant};

use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};

use serde::{Deserialize, Serialize};

/// A seeded pseudo-random number generator (xorshift64).
/// Deterministic, fast, no external dependencies.
#[derive(Debug, Clone)]
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    /// Create a new PRNG with the given seed. A zero seed is replaced with 1.
    pub fn new(seed: u64) -> Self {
        // Ensure non-zero state
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    /// Generate next pseudo-random u64.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Generate a random number in [0, bound).
    pub fn next_bound(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() as usize) % bound
    }

    /// Return true with probability `p` (0.0 to 1.0).
    pub fn chance(&mut self, p: f64) -> bool {
        let threshold = (p * u64::MAX as f64) as u64;
        self.next_u64() < threshold
    }

    /// Get the current seed state (for logging/replay).
    pub fn seed_state(&self) -> u64 {
        self.state
    }
}

/// Simulation time (logical ticks, not wall clock).
pub type SimTime = u64;

/// A message in the simulation.
#[derive(Debug, Clone)]
pub struct SimMessage {
    /// Source actor.
    pub from: String,
    /// Destination actor.
    pub to: String,
    /// Message type name.
    pub msg_type: String,
    /// Serialized payload.
    pub payload: String,
    /// When this message should be delivered (logical time).
    pub deliver_at: SimTime,
    /// Unique message ID.
    pub id: u64,
}

impl PartialEq for SimMessage {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for SimMessage {}

impl PartialOrd for SimMessage {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SimMessage {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; we want min-heap (earliest delivery first)
        other
            .deliver_at
            .cmp(&self.deliver_at)
            .then_with(|| other.id.cmp(&self.id))
    }
}

/// Fault injection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultConfig {
    /// Probability of delaying a message (0.0 to 1.0).
    pub message_delay_prob: f64,
    /// Max delay ticks when a message is delayed.
    pub max_delay_ticks: u64,
    /// Probability of dropping a message entirely.
    pub message_drop_prob: f64,
    /// Probability of crashing an actor after processing a message.
    pub actor_crash_prob: f64,
    /// Probability of restarting a crashed actor.
    pub actor_restart_prob: f64,
}

impl Default for FaultConfig {
    fn default() -> Self {
        Self {
            message_delay_prob: 0.0,
            message_drop_prob: 0.0,
            actor_crash_prob: 0.0,
            actor_restart_prob: 0.0,
            max_delay_ticks: 10,
        }
    }
}

impl FaultConfig {
    /// No faults — pure deterministic ordering.
    pub fn none() -> Self {
        Self::default()
    }

    /// Light faults — occasional delays, no drops or crashes.
    pub fn light() -> Self {
        Self {
            message_delay_prob: 0.1,
            max_delay_ticks: 5,
            message_drop_prob: 0.0,
            actor_crash_prob: 0.0,
            actor_restart_prob: 0.0,
        }
    }

    /// Heavy faults — frequent delays, occasional drops and crashes.
    pub fn heavy() -> Self {
        Self {
            message_delay_prob: 0.3,
            max_delay_ticks: 20,
            message_drop_prob: 0.05,
            actor_crash_prob: 0.02,
            actor_restart_prob: 0.8,
        }
    }
}

/// Actor state in the simulation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimActorState {
    /// Actor is running and can process messages.
    Running,
    /// Actor has crashed and will not process messages until restarted.
    Crashed,
}

/// The deterministic simulation scheduler.
///
/// Drives message delivery in a controlled, reproducible order.
/// All "concurrency" is simulated — there are no real threads.
pub struct SimScheduler {
    /// The PRNG controlling all non-determinism.
    rng: DeterministicRng,
    /// Current logical time.
    current_time: SimTime,
    /// Priority queue of pending messages (ordered by delivery time).
    pending: BinaryHeap<SimMessage>,
    /// Per-actor mailbox of delivered (ready to process) messages.
    /// BTreeMap ensures deterministic iteration order.
    mailboxes: BTreeMap<String, VecDeque<SimMessage>>,
    /// Actor states. BTreeMap ensures deterministic iteration order
    /// (critical for reproducible crash selection).
    actor_states: BTreeMap<String, SimActorState>,
    /// Fault injection config.
    fault_config: FaultConfig,
    /// Next message ID.
    next_msg_id: u64,
    /// Messages that were dropped (for inspection).
    dropped: Vec<SimMessage>,
    /// Messages that were delivered (for inspection).
    delivered: Vec<SimMessage>,
    /// Total ticks executed.
    ticks: u64,
}

impl SimScheduler {
    /// Create a new simulation scheduler with the given seed and fault config.
    pub fn new(seed: u64, fault_config: FaultConfig) -> Self {
        Self {
            rng: DeterministicRng::new(seed),
            current_time: 0,
            pending: BinaryHeap::new(),
            mailboxes: BTreeMap::new(),
            actor_states: BTreeMap::new(),
            fault_config,
            next_msg_id: 0,
            dropped: Vec::new(),
            delivered: Vec::new(),
            ticks: 0,
        }
    }

    /// Register an actor in the simulation.
    pub fn register_actor(&mut self, actor_id: &str) {
        self.actor_states
            .insert(actor_id.to_string(), SimActorState::Running);
        self.mailboxes.entry(actor_id.to_string()).or_default();
    }

    /// Send a message. It enters the pending queue and may be subject to faults.
    pub fn send(&mut self, from: &str, to: &str, msg_type: &str, payload: &str) {
        let id = self.next_msg_id;
        self.next_msg_id += 1;

        // Apply fault injection
        if self.rng.chance(self.fault_config.message_drop_prob) {
            // Drop the message
            self.dropped.push(SimMessage {
                from: from.to_string(),
                to: to.to_string(),
                msg_type: msg_type.to_string(),
                payload: payload.to_string(),
                deliver_at: self.current_time,
                id,
            });
            return;
        }

        let delay = if self.rng.chance(self.fault_config.message_delay_prob) {
            1 + self
                .rng
                .next_bound(self.fault_config.max_delay_ticks as usize) as u64
        } else {
            1 // Deliver on next tick
        };

        let msg = SimMessage {
            from: from.to_string(),
            to: to.to_string(),
            msg_type: msg_type.to_string(),
            payload: payload.to_string(),
            deliver_at: self.current_time + delay,
            id,
        };

        self.pending.push(msg);
    }

    /// Send a message with an explicit delivery time (for scheduled actions).
    ///
    /// Unlike [`send()`], this bypasses fault injection delay — the delay is
    /// intentional, not a fault. Message drop and crash faults still apply.
    pub fn send_at(
        &mut self,
        from: &str,
        to: &str,
        msg_type: &str,
        payload: &str,
        deliver_at: SimTime,
    ) {
        let id = self.next_msg_id;
        self.next_msg_id += 1;

        // Apply message drop fault (timer delivery is not guaranteed).
        if self.rng.chance(self.fault_config.message_drop_prob) {
            self.dropped.push(SimMessage {
                from: from.to_string(),
                to: to.to_string(),
                msg_type: msg_type.to_string(),
                payload: payload.to_string(),
                deliver_at,
                id,
            });
            return;
        }

        self.pending.push(SimMessage {
            from: from.to_string(),
            to: to.to_string(),
            msg_type: msg_type.to_string(),
            payload: payload.to_string(),
            deliver_at,
            id,
        });
    }

    /// Advance one tick: deliver all messages due at current_time + 1.
    /// Returns the messages delivered this tick.
    pub fn tick(&mut self) -> Vec<SimMessage> {
        self.current_time += 1;
        self.ticks += 1;
        let mut delivered_this_tick = Vec::new();

        // Deliver all messages due at or before current time
        while let Some(msg) = self.pending.peek() {
            if msg.deliver_at <= self.current_time {
                let msg = self.pending.pop().unwrap(); // ci-ok: guarded by peek() above
                let to = msg.to.clone();

                // Check if target actor is running
                let actor_state = self.actor_states.get(&to).cloned();
                match actor_state {
                    Some(SimActorState::Running) => {
                        self.mailboxes.entry(to).or_default().push_back(msg.clone());
                        delivered_this_tick.push(msg.clone());
                        self.delivered.push(msg);
                    }
                    Some(SimActorState::Crashed) => {
                        // Actor is crashed — message is lost (or could be re-queued)
                        self.dropped.push(msg);

                        // Maybe restart the actor
                        if self.rng.chance(self.fault_config.actor_restart_prob) {
                            self.actor_states.insert(to, SimActorState::Running);
                        }
                    }
                    None => {
                        // Unknown actor — drop
                        self.dropped.push(msg);
                    }
                }
            } else {
                break;
            }
        }

        // Maybe crash an actor after delivery
        if self.rng.chance(self.fault_config.actor_crash_prob) {
            let running: Vec<String> = self
                .actor_states
                .iter()
                .filter(|(_, s)| **s == SimActorState::Running)
                .map(|(k, _)| k.clone())
                .collect();
            if !running.is_empty() {
                let idx = self.rng.next_bound(running.len());
                self.actor_states
                    .insert(running[idx].clone(), SimActorState::Crashed);
            }
        }

        delivered_this_tick
    }

    /// Take the next message from an actor's mailbox.
    pub fn receive(&mut self, actor_id: &str) -> Option<SimMessage> {
        self.mailboxes.get_mut(actor_id).and_then(|q| q.pop_front())
    }

    /// Check if the simulation has no more pending messages.
    pub fn is_quiescent(&self) -> bool {
        self.pending.is_empty() && self.mailboxes.values().all(|q| q.is_empty())
    }

    /// Run until quiescent or max ticks reached. Returns total ticks.
    pub fn run_until_quiescent(&mut self, max_ticks: u64) -> u64 {
        for _ in 0..max_ticks {
            if self.is_quiescent() {
                break;
            }
            self.tick();
        }
        self.ticks
    }

    /// Get the current logical time.
    pub fn current_time(&self) -> SimTime {
        self.current_time
    }

    /// Get total messages delivered.
    pub fn total_delivered(&self) -> usize {
        self.delivered.len()
    }

    /// Get total messages dropped.
    pub fn total_dropped(&self) -> usize {
        self.dropped.len()
    }

    /// Get the delivered messages log (for assertions).
    pub fn delivered_log(&self) -> &[SimMessage] {
        &self.delivered
    }

    /// Get the dropped messages log.
    pub fn dropped_log(&self) -> &[SimMessage] {
        &self.dropped
    }

    /// Get an actor's current state.
    pub fn actor_state(&self, actor_id: &str) -> Option<&SimActorState> {
        self.actor_states.get(actor_id)
    }

    /// Get the seed state for replay logging.
    pub fn seed_state(&self) -> u64 {
        self.rng.seed_state()
    }

    /// Get mailbox depth for an actor.
    pub fn mailbox_depth(&self, actor_id: &str) -> usize {
        self.mailboxes.get(actor_id).map_or(0, |q| q.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deterministic_rng_is_reproducible() {
        let mut rng1 = DeterministicRng::new(42);
        let mut rng2 = DeterministicRng::new(42);

        let seq1: Vec<u64> = (0..10).map(|_| rng1.next_u64()).collect();
        let seq2: Vec<u64> = (0..10).map(|_| rng2.next_u64()).collect();
        assert_eq!(seq1, seq2, "Same seed must produce same sequence");
    }

    #[test]
    fn test_different_seeds_produce_different_sequences() {
        let mut rng1 = DeterministicRng::new(42);
        let mut rng2 = DeterministicRng::new(123);

        let v1 = rng1.next_u64();
        let v2 = rng2.next_u64();
        assert_ne!(v1, v2);
    }

    #[test]
    fn test_basic_message_delivery() {
        let mut sched = SimScheduler::new(1, FaultConfig::none());
        sched.register_actor("actor-a");
        sched.register_actor("actor-b");

        sched.send("actor-a", "actor-b", "Ping", "{}");
        assert_eq!(sched.total_delivered(), 0);

        sched.tick(); // deliver
        assert_eq!(sched.total_delivered(), 1);

        let msg = sched.receive("actor-b").unwrap();
        assert_eq!(msg.msg_type, "Ping");
        assert_eq!(msg.from, "actor-a");
    }

    #[test]
    fn test_message_ordering_is_deterministic() {
        // Run the same scenario twice with the same seed → same delivery order
        fn run_scenario(seed: u64) -> Vec<String> {
            let mut sched = SimScheduler::new(seed, FaultConfig::light());
            sched.register_actor("a");
            sched.register_actor("b");

            for i in 0..10 {
                sched.send("a", "b", &format!("msg-{i}"), "{}");
            }

            sched.run_until_quiescent(100);

            sched
                .delivered_log()
                .iter()
                .map(|m| m.msg_type.clone())
                .collect()
        }

        let run1 = run_scenario(42);
        let run2 = run_scenario(42);
        assert_eq!(run1, run2, "Same seed must produce same delivery order");
    }

    #[test]
    fn test_different_seeds_may_produce_different_order() {
        fn run_scenario(seed: u64) -> Vec<String> {
            let mut sched = SimScheduler::new(seed, FaultConfig::light());
            sched.register_actor("a");
            sched.register_actor("b");

            for i in 0..20 {
                sched.send("a", "b", &format!("msg-{i}"), "{}");
            }

            sched.run_until_quiescent(100);
            sched
                .delivered_log()
                .iter()
                .map(|m| m.msg_type.clone())
                .collect()
        }

        let run1 = run_scenario(42);
        let run2 = run_scenario(999);
        // With light faults (10% delay), different seeds should likely produce different orders
        // This isn't guaranteed for every pair, but is overwhelmingly likely with 20 messages
        assert_ne!(
            run1, run2,
            "Different seeds should usually produce different orders"
        );
    }

    #[test]
    fn test_fault_injection_message_drop() {
        let config = FaultConfig {
            message_drop_prob: 1.0, // Drop everything
            ..FaultConfig::none()
        };
        let mut sched = SimScheduler::new(42, config);
        sched.register_actor("a");
        sched.register_actor("b");

        sched.send("a", "b", "Important", "{}");
        sched.tick();

        assert_eq!(sched.total_delivered(), 0);
        assert_eq!(sched.total_dropped(), 1);
    }

    #[test]
    fn test_fault_injection_actor_crash() {
        let config = FaultConfig {
            actor_crash_prob: 1.0, // Crash after every tick
            ..FaultConfig::none()
        };
        let mut sched = SimScheduler::new(42, config);
        sched.register_actor("a");
        sched.register_actor("b");

        sched.send("a", "b", "msg", "{}");
        sched.tick();

        // Message should be delivered (crash happens AFTER delivery)
        assert_eq!(sched.total_delivered(), 1);

        // But one of the actors should now be crashed
        let crashed = sched
            .actor_states
            .values()
            .filter(|s| **s == SimActorState::Crashed)
            .count();
        assert!(crashed > 0, "Should have at least one crashed actor");
    }

    #[test]
    fn test_message_to_crashed_actor_is_dropped() {
        let mut sched = SimScheduler::new(42, FaultConfig::none());
        sched.register_actor("a");
        sched.register_actor("b");

        // Manually crash actor-b
        sched
            .actor_states
            .insert("b".to_string(), SimActorState::Crashed);

        sched.send("a", "b", "msg", "{}");
        sched.tick();

        assert_eq!(sched.total_delivered(), 0);
        assert_eq!(sched.total_dropped(), 1);
    }

    #[test]
    fn test_quiescence_detection() {
        let mut sched = SimScheduler::new(1, FaultConfig::none());
        sched.register_actor("a");

        assert!(sched.is_quiescent());

        sched.send("a", "a", "self-msg", "{}");
        assert!(!sched.is_quiescent());

        sched.tick();
        // Message delivered to mailbox — not quiescent until consumed
        sched.receive("a");
        assert!(sched.is_quiescent());
    }

    #[test]
    fn test_run_until_quiescent() {
        let mut sched = SimScheduler::new(1, FaultConfig::none());
        sched.register_actor("a");
        sched.register_actor("b");

        sched.send("a", "b", "msg-1", "{}");
        sched.send("a", "b", "msg-2", "{}");
        sched.send("a", "b", "msg-3", "{}");

        let ticks = sched.run_until_quiescent(100);
        assert!(ticks <= 100);
        assert_eq!(sched.total_delivered(), 3);
    }

    #[test]
    fn test_message_delay_increases_delivery_time() {
        let config = FaultConfig {
            message_delay_prob: 1.0, // Always delay
            max_delay_ticks: 5,
            ..FaultConfig::none()
        };
        let mut sched = SimScheduler::new(42, config);
        sched.register_actor("a");
        sched.register_actor("b");

        sched.send("a", "b", "delayed", "{}");

        // Tick 1: message not yet delivered (delayed)
        sched.tick();
        let delivered_at_1 = sched.total_delivered();

        // Run more ticks
        sched.run_until_quiescent(20);
        assert_eq!(
            sched.total_delivered(),
            1,
            "Message should eventually arrive"
        );
        if delivered_at_1 == 0 {
            assert!(
                sched.current_time() > 1,
                "Delivery should be delayed beyond tick 1"
            );
        }
    }

    #[test]
    fn test_heavy_faults_simulation_completes() {
        // Even with heavy faults, simulation should complete without panic
        let mut sched = SimScheduler::new(12345, FaultConfig::heavy());
        for i in 0..5 {
            sched.register_actor(&format!("actor-{i}"));
        }

        // Send 50 messages between random actors
        let mut rng = DeterministicRng::new(67890);
        for _ in 0..50 {
            let from = format!("actor-{}", rng.next_bound(5));
            let to = format!("actor-{}", rng.next_bound(5));
            sched.send(&from, &to, "msg", "{}");
        }

        sched.run_until_quiescent(200);

        // Just verify it completed without panic and some messages got through
        let total = sched.total_delivered() + sched.total_dropped();
        assert!(total > 0, "Should have processed some messages");
    }

    #[test]
    fn test_send_at_delivers_at_specified_time() {
        let mut sched = SimScheduler::new(1, FaultConfig::none());
        sched.register_actor("a");
        sched.register_actor("b");

        // Schedule a message at time 5
        sched.send_at("a", "b", "Scheduled", "{}", 5);

        // Ticks 1-4: nothing delivered
        for _ in 1..5 {
            sched.tick();
            assert_eq!(
                sched.total_delivered(),
                0,
                "should not deliver before deliver_at"
            );
        }

        // Tick 5: message delivered
        sched.tick();
        assert_eq!(sched.total_delivered(), 1);

        let msg = sched.receive("b").unwrap();
        assert_eq!(msg.msg_type, "Scheduled");
        assert_eq!(msg.deliver_at, 5);
    }

    #[test]
    fn test_send_at_respects_message_drop() {
        let config = FaultConfig {
            message_drop_prob: 1.0,
            ..FaultConfig::none()
        };
        let mut sched = SimScheduler::new(42, config);
        sched.register_actor("a");
        sched.register_actor("b");

        sched.send_at("a", "b", "Scheduled", "{}", 3);
        sched.run_until_quiescent(10);

        assert_eq!(sched.total_delivered(), 0);
        assert_eq!(sched.total_dropped(), 1);
    }
}
