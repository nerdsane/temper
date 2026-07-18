//! Full deterministic run recording.

use std::collections::BTreeMap;

use super::{RunRecord, SimActorResult, SimActorSystem};

impl SimActorSystem {
    pub(super) fn result_snapshot(&self) -> SimActorResult {
        let actor_states = self
            .actors
            .iter()
            .map(|(id, handler)| {
                (
                    id.clone(),
                    handler.current_status(),
                    handler.current_item_count(),
                    handler.event_count(),
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
            actor_states,
        }
    }

    /// Run random exploration and return a full [`RunRecord`] alongside the result.
    ///
    /// The record captures every transition, event, and final state. Two calls
    /// with the same seed must produce identical records.
    pub fn run_random_recorded(&mut self) -> (SimActorResult, RunRecord) {
        let result = self.run_random();

        let events: BTreeMap<String, Vec<String>> = self
            .actors
            .iter()
            .map(|(id, handler)| {
                let event_strings = match handler.events_json() {
                    serde_json::Value::Array(events) => events
                        .iter()
                        .map(|event| serde_json::to_string(event).unwrap_or_default())
                        .collect(),
                    _ => Vec::new(),
                };
                (id.clone(), event_strings)
            })
            .collect();

        let final_states = self
            .actors
            .iter()
            .map(|(id, handler)| {
                let counters_json =
                    serde_json::to_string(&handler.events_json()).unwrap_or_default();
                (
                    id.clone(),
                    handler.current_status(),
                    handler.current_item_count(),
                    handler.event_count(),
                    counters_json,
                )
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
}
