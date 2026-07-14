//! The deterministic simulation scheduler — message delivery and fault injection.

use std::collections::{BTreeMap, BinaryHeap, VecDeque};

use super::rng::DeterministicRng;
use super::types::{FaultConfig, SimActorState, SimMessage, SimTime};

const DEFAULT_MAILBOX_BUDGET_PER_ACTOR: usize = 4_096;

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
    /// Next mailbox position for deterministic, starvation-free draining.
    next_mailbox_index: usize,
    /// Maximum ready messages retained for one actor before failing fast.
    mailbox_budget_per_actor: usize,
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
        Self::with_mailbox_budget(seed, fault_config, DEFAULT_MAILBOX_BUDGET_PER_ACTOR)
    }

    /// Create a scheduler with an explicit per-actor ready-mailbox budget.
    pub fn with_mailbox_budget(
        seed: u64,
        fault_config: FaultConfig,
        mailbox_budget_per_actor: usize,
    ) -> Self {
        assert!(
            mailbox_budget_per_actor > 0,
            "mailbox budget per actor must be positive"
        );
        Self {
            rng: DeterministicRng::new(seed),
            current_time: 0,
            pending: BinaryHeap::new(),
            mailboxes: BTreeMap::new(),
            next_mailbox_index: 0,
            mailbox_budget_per_actor,
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

    /// Advance one tick and enqueue every message now due in its target mailbox.
    ///
    /// [`drain_ready`](Self::drain_ready) is the sole ownership transfer from
    /// scheduler mailboxes to a simulation driver.
    pub fn tick(&mut self) {
        self.current_time += 1;
        self.ticks += 1;

        // Deliver all messages due at or before current time
        while let Some(msg) = self.pending.peek() {
            if msg.deliver_at <= self.current_time {
                let msg = self.pending.pop().unwrap(); // ci-ok: guarded by peek() above
                let to = msg.to.clone();

                // Check if target actor is running
                let actor_state = self.actor_states.get(&to).cloned();
                match actor_state {
                    Some(SimActorState::Running) => {
                        let mailbox = self.mailboxes.entry(to.clone()).or_default();
                        assert!(
                            mailbox.len() < self.mailbox_budget_per_actor,
                            "ready mailbox budget exhausted for actor '{to}'"
                        );
                        mailbox.push_back(msg.clone());
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
    }

    /// Remove up to `message_budget` ready messages in deterministic order.
    ///
    /// Actors are visited in cyclic lexicographic [`BTreeMap`] order and each
    /// actor's messages retain FIFO order. A drained message cannot be returned
    /// again, and a small budget cannot permanently starve a later mailbox.
    pub fn drain_ready(&mut self, message_budget: usize) -> Vec<SimMessage> {
        assert!(message_budget > 0, "message budget must be positive");

        let actor_ids: Vec<String> = self.mailboxes.keys().cloned().collect();
        if actor_ids.is_empty() {
            return Vec::new();
        }

        let mut ready = Vec::new();
        let mut index = self.next_mailbox_index % actor_ids.len();
        let mut empty_mailboxes_seen = 0;
        while ready.len() < message_budget && empty_mailboxes_seen < actor_ids.len() {
            let actor_id = &actor_ids[index];
            let mailbox = self.mailboxes.get_mut(actor_id).unwrap(); // ci-ok: id came from keys
            if let Some(message) = mailbox.pop_front() {
                ready.push(message);
                empty_mailboxes_seen = 0;
            } else {
                empty_mailboxes_seen += 1;
            }
            index = (index + 1) % actor_ids.len();
        }
        self.next_mailbox_index = index;

        debug_assert!(ready.len() <= message_budget);
        ready
    }

    /// Take the next message from one actor's mailbox.
    ///
    /// Simulation drivers should prefer [`drain_ready`](Self::drain_ready) so
    /// ordering and budgets are shared. This actor-specific consumer remains
    /// available for direct scheduler users.
    pub fn receive(&mut self, actor_id: &str) -> Option<SimMessage> {
        self.mailboxes
            .get_mut(actor_id)
            .and_then(VecDeque::pop_front)
    }

    /// Check if the simulation has no more pending messages.
    pub fn is_quiescent(&self) -> bool {
        self.pending.is_empty() && self.mailboxes.values().all(|q| q.is_empty())
    }

    /// Advance until quiescent or `max_ticks` is reached.
    ///
    /// Ready mailbox messages remain owned by the scheduler for a caller to
    /// consume with [`receive`](Self::receive) or [`drain_ready`](Self::drain_ready).
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
#[path = "core/tests.rs"]
mod tests;
