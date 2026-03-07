//! Core simulation types: messages, fault configuration, and actor state.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: u64, deliver_at: SimTime) -> SimMessage {
        SimMessage {
            from: "a".into(),
            to: "b".into(),
            msg_type: "test".into(),
            payload: "{}".into(),
            deliver_at,
            id,
        }
    }

    #[test]
    fn sim_message_eq_by_id() {
        let m1 = msg(1, 10);
        let m2 = msg(1, 20);
        assert_eq!(m1, m2); // same id, different deliver_at
    }

    #[test]
    fn sim_message_ne_different_id() {
        let m1 = msg(1, 10);
        let m2 = msg(2, 10);
        assert_ne!(m1, m2);
    }

    #[test]
    fn sim_message_ord_earlier_delivery_is_greater() {
        // BinaryHeap max-heap, so earlier delivery = "greater" for min-heap behavior
        let early = msg(1, 5);
        let late = msg(2, 10);
        assert!(early > late); // reversed ordering for min-heap
    }

    #[test]
    fn sim_message_ord_same_time_breaks_by_id() {
        let m1 = msg(1, 10);
        let m2 = msg(2, 10);
        assert!(m1 > m2); // lower id popped first (reversed)
    }

    #[test]
    fn sim_message_min_heap_via_binary_heap() {
        use std::collections::BinaryHeap;
        let mut heap = BinaryHeap::new();
        heap.push(msg(3, 15));
        heap.push(msg(1, 5));
        heap.push(msg(2, 10));
        // Min-heap: earliest delivery_at should pop first
        assert_eq!(heap.pop().unwrap().deliver_at, 5);
        assert_eq!(heap.pop().unwrap().deliver_at, 10);
        assert_eq!(heap.pop().unwrap().deliver_at, 15);
    }

    #[test]
    fn fault_config_none_is_default() {
        let none = FaultConfig::none();
        let default = FaultConfig::default();
        assert_eq!(none.message_delay_prob, default.message_delay_prob);
        assert_eq!(none.message_drop_prob, 0.0);
        assert_eq!(none.actor_crash_prob, 0.0);
    }

    #[test]
    fn fault_config_light_has_delays_only() {
        let light = FaultConfig::light();
        assert!(light.message_delay_prob > 0.0);
        assert_eq!(light.message_drop_prob, 0.0);
        assert_eq!(light.actor_crash_prob, 0.0);
    }

    #[test]
    fn fault_config_heavy_has_all_faults() {
        let heavy = FaultConfig::heavy();
        assert!(heavy.message_delay_prob > 0.0);
        assert!(heavy.message_drop_prob > 0.0);
        assert!(heavy.actor_crash_prob > 0.0);
        assert!(heavy.actor_restart_prob > 0.0);
    }

    #[test]
    fn fault_config_serde_roundtrip() {
        let cfg = FaultConfig::heavy();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: FaultConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.message_delay_prob, cfg.message_delay_prob);
        assert_eq!(back.max_delay_ticks, cfg.max_delay_ticks);
    }

    #[test]
    fn sim_actor_state_equality() {
        assert_eq!(SimActorState::Running, SimActorState::Running);
        assert_eq!(SimActorState::Crashed, SimActorState::Crashed);
        assert_ne!(SimActorState::Running, SimActorState::Crashed);
    }
}
