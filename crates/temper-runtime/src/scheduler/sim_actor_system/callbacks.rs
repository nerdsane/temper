//! Deterministic integration callback configuration and delivery.

use std::collections::BTreeMap;

use super::SimActorSystem;
use crate::scheduler::SimClock;

/// Configures how integration callbacks are delivered in simulation.
#[derive(Debug, Clone, Default)]
pub struct SimIntegrationResponses {
    responses: BTreeMap<(String, String), String>,
}

impl SimIntegrationResponses {
    /// Create an empty integration response map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure a callback action for a trigger.
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
            .map(String::as_str)
    }
}

/// A simulation-driver failure that invalidates a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimExecutionError {
    /// Actor whose action failed.
    pub actor_id: String,
    /// Action that failed.
    pub action: String,
    /// Error returned by the actor handler or driver budget.
    pub description: String,
    /// Logical tick at failure.
    pub tick: u64,
}

impl SimActorSystem {
    pub(super) fn schedule_integration_callbacks(&mut self, actor_id: &str) {
        let Some(handler) = self.actors.get(actor_id) else {
            return;
        };

        for trigger in handler.pending_callbacks() {
            let callback_action = self
                .integration_responses
                .get_callback(actor_id, &trigger)
                .or_else(|| {
                    actor_id.find(':').and_then(|colon| {
                        self.integration_responses
                            .get_callback(&actor_id[..colon], &trigger)
                    })
                });

            if let Some(callback_action) = callback_action {
                self.pending_integration_callbacks
                    .push_back((actor_id.to_string(), callback_action.to_string()));
            }
        }
    }

    pub(super) fn deliver_integration_callbacks(
        &mut self,
        reactions: &mut usize,
    ) -> Result<(), String> {
        while let Some((actor_id, callback_action)) = self.pending_integration_callbacks.pop_front()
        {
            if *reactions == self.config.reaction_budget_per_tick {
                self.pending_integration_callbacks
                    .push_front((actor_id.clone(), callback_action.clone()));
                let description =
                    format!("integration callback budget exhausted after {reactions} reactions");
                self.execution_errors.push(SimExecutionError {
                    actor_id,
                    action: callback_action,
                    description: description.clone(),
                    tick: self.clock.tick(),
                });
                return Err(description);
            }

            *reactions += 1;
            if let Err(error) = self.apply_action(&actor_id, &callback_action, "{}") {
                let description = format!(
                    "integration callback '{callback_action}' failed for '{actor_id}': {error}"
                );
                self.execution_errors.push(SimExecutionError {
                    actor_id,
                    action: callback_action,
                    description: description.clone(),
                    tick: self.clock.tick(),
                });
                return Err(description);
            }
        }
        Ok(())
    }
}
