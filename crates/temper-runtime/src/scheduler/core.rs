//! The deterministic simulation scheduler — message delivery and fault injection.

use std::collections::{BTreeMap, BinaryHeap, VecDeque};

use super::rng::DeterministicRng;
use super::types::{FaultConfig, SimActorState, SimMessage, SimTime};

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
    /// Actors that crossed from crashed to running during the latest ticks.
    restarted: Vec<String>,
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
            restarted: Vec::new(),
        }
    }

    /// Register an actor in the simulation.
    pub fn register_actor(&mut self, actor_id: &str) {
        self.actor_states
            .insert(actor_id.to_string(), SimActorState::Running);
        self.mailboxes.entry(actor_id.to_string()).or_default();
    }

    /// Crash one registered actor at an explicit deterministic fault point.
    pub fn crash_actor(&mut self, actor_id: &str) {
        let state = self
            .actor_states
            .get_mut(actor_id)
            .unwrap_or_else(|| panic!("cannot crash unknown actor '{actor_id}'"));
        *state = SimActorState::Crashed;
    }

    /// Restart one crashed actor and report the recovery edge to the harness.
    pub fn restart_actor(&mut self, actor_id: &str) {
        let state = self
            .actor_states
            .get_mut(actor_id)
            .unwrap_or_else(|| panic!("cannot restart unknown actor '{actor_id}'"));
        assert_eq!(
            *state,
            SimActorState::Crashed,
            "actor must be crashed first"
        );
        *state = SimActorState::Running;
        self.restarted.push(actor_id.to_string());
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

    /// Advance one tick and enqueue every message due at the new logical time.
    ///
    /// This method only transfers ownership from the pending queue to target
    /// mailboxes. Drivers consume deliveries through [`Self::drain_ready`] or
    /// [`Self::receive`], so a processable message is never exposed as a clone.
    pub fn tick(&mut self) {
        self.current_time += 1;
        self.ticks += 1;

        // Restart is an independent per-tick fault edge. Tying restart only
        // to a newly due message can strand a crashed, idle actor forever.
        let crashed: Vec<String> = self
            .actor_states
            .iter()
            .filter(|(_, state)| **state == SimActorState::Crashed)
            .map(|(actor_id, _)| actor_id.clone())
            .collect();
        for actor_id in crashed {
            if self.rng.chance(self.fault_config.actor_restart_prob) {
                self.actor_states
                    .insert(actor_id.clone(), SimActorState::Running);
                self.restarted.push(actor_id);
            }
        }

        // Enqueue all messages due at or before current time.
        while let Some(msg) = self.pending.peek() {
            if msg.deliver_at <= self.current_time {
                let msg = self.pending.pop().unwrap(); // ci-ok: guarded by peek() above
                let to = msg.to.clone();

                // Check if target actor is running
                let actor_state = self.actor_states.get(&to).cloned();
                match actor_state {
                    Some(SimActorState::Running) => {
                        self.delivered.push(msg.clone());
                        self.mailboxes.entry(to).or_default().push_back(msg);
                    }
                    Some(SimActorState::Crashed) => {
                        // Actor remained crashed through this tick — drop.
                        self.dropped.push(msg);
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
    }

    /// Complete a tick after the driver has applied every drained delivery.
    ///
    /// Crash injection is deliberately separated from [`Self::tick`]: a
    /// message enqueued while its target is running must be consumed before a
    /// post-delivery crash can change that actor's state.
    pub fn finish_tick(&mut self) {
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
    }

    /// Remove every ready message in actor-id order and FIFO mailbox order.
    ///
    /// A drained message leaves the scheduler and is owned by the caller,
    /// which must either apply it or report why application failed.
    pub fn drain_ready(&mut self) -> Vec<SimMessage> {
        let mut ready = Vec::new();
        for mailbox in self.mailboxes.values_mut() {
            ready.extend(mailbox.drain(..));
        }
        ready
    }

    /// Take the next message from an actor's mailbox.
    pub fn receive(&mut self, actor_id: &str) -> Option<SimMessage> {
        self.mailboxes.get_mut(actor_id).and_then(|q| q.pop_front())
    }

    /// Check if the simulation has no more pending messages.
    pub fn is_quiescent(&self) -> bool {
        self.pending.is_empty() && self.mailboxes.values().all(|q| q.is_empty())
    }

    /// Whether an actor already owns an unconsumed pending or mailbox message.
    pub fn has_in_flight(&self, actor_id: &str) -> bool {
        self.pending.iter().any(|message| message.to == actor_id)
            || self
                .mailboxes
                .get(actor_id)
                .is_some_and(|mailbox| !mailbox.is_empty())
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

    /// Drain actor IDs that restarted since the previous observation.
    pub fn take_restarted_actors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.restarted)
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

            for _ in 0..100 {
                if sched.pending.is_empty() {
                    break;
                }
                sched.tick();
            }

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

            for _ in 0..100 {
                if sched.pending.is_empty() {
                    break;
                }
                sched.tick();
            }
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
        sched.drain_ready();
        sched.finish_tick();

        // Message is consumed before the post-delivery crash.
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
    fn message_to_unknown_actor_is_dropped_without_a_mailbox() {
        let mut sched = SimScheduler::new(42, FaultConfig::none());
        sched.register_actor("source");

        sched.send("source", "missing", "msg", "{}");
        sched.tick();

        assert_eq!(sched.total_dropped(), 1);
        assert!(sched.drain_ready().is_empty());
        assert!(sched.is_quiescent());
    }

    #[test]
    fn duplicate_sends_are_distinct_and_each_drained_once() {
        let mut sched = SimScheduler::new(42, FaultConfig::none());
        sched.register_actor("source");
        sched.register_actor("target");

        sched.send("source", "target", "same", "{}");
        sched.send("source", "target", "same", "{}");
        sched.tick();

        let ready = sched.drain_ready();
        assert_eq!(ready.len(), 2);
        assert_ne!(ready[0].id, ready[1].id);
        assert!(sched.drain_ready().is_empty());
        assert!(sched.is_quiescent());
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
    fn drain_ready_consumes_all_mailboxes_deterministically() {
        let mut sched = SimScheduler::new(1, FaultConfig::none());
        sched.register_actor("a");
        sched.register_actor("b");

        sched.send("a", "b", "b-1", "{}");
        sched.send("b", "a", "a-1", "{}");
        sched.send("a", "b", "b-2", "{}");

        sched.tick();
        let ready = sched.drain_ready();
        let delivered: Vec<_> = ready
            .iter()
            .map(|message| message.msg_type.as_str())
            .collect();
        assert_eq!(delivered, ["a-1", "b-1", "b-2"]);
        assert_eq!(sched.total_delivered(), 3);
        assert!(sched.is_quiescent());
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
        for _ in 0..20 {
            if sched.total_delivered() == 1 {
                break;
            }
            sched.tick();
        }
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
        let mut rng = super::super::DeterministicRng::new(67890);
        for _ in 0..50 {
            let from = format!("actor-{}", rng.next_bound(5));
            let to = format!("actor-{}", rng.next_bound(5));
            sched.send(&from, &to, "msg", "{}");
        }

        for _ in 0..200 {
            if sched.pending.is_empty() {
                break;
            }
            sched.tick();
            sched.drain_ready();
        }

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
        for _ in 0..10 {
            if sched.pending.is_empty() {
                break;
            }
            sched.tick();
        }

        assert_eq!(sched.total_delivered(), 0);
        assert_eq!(sched.total_dropped(), 1);
    }
}
