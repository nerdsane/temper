use super::*;

struct CallbackFailureHandler {
    status: String,
    pending_callbacks: Vec<String>,
}

impl CallbackFailureHandler {
    fn new() -> Self {
        Self {
            status: "Ready".to_string(),
            pending_callbacks: Vec::new(),
        }
    }
}

impl SimActorHandler for CallbackFailureHandler {
    fn init(&mut self) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({"status": self.status}))
    }

    fn handle_message(&mut self, action: &str, _params: &str) -> Result<serde_json::Value, String> {
        match action {
            "Start" => {
                self.status = "Started".to_string();
                self.pending_callbacks = vec!["integration".to_string()];
                Ok(serde_json::json!({"status": self.status}))
            }
            "Callback" => Err("callback rejected".to_string()),
            "Complete" => {
                self.pending_callbacks.clear();
                Ok(serde_json::json!({"status": self.status}))
            }
            "Loop" => Ok(serde_json::json!({"status": self.status})),
            _ => Err(format!("unknown action: {action}")),
        }
    }

    fn current_status(&self) -> String {
        self.status.clone()
    }

    fn current_item_count(&self) -> usize {
        0
    }

    fn event_count(&self) -> usize {
        0
    }

    fn valid_actions(&self) -> Vec<String> {
        if self.status == "Ready" {
            vec!["Start".to_string()]
        } else {
            Vec::new()
        }
    }

    fn events_json(&self) -> serde_json::Value {
        serde_json::json!([])
    }

    fn pending_callbacks(&self) -> Vec<String> {
        self.pending_callbacks.clone()
    }
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
    let sim = SimActorSystem::new(config);
    assert_eq!(sim.message_batch_budget, 1_024);
    assert_eq!(sim.reaction_budget_per_tick, 1_024);
}

#[test]
fn callback_failure_is_returned_and_invalidates_random_run() {
    let config = SimActorSystemConfig {
        seed: 1,
        max_ticks: 2,
        faults: FaultConfig {
            message_delay_prob: 1.0,
            max_delay_ticks: 2,
            ..FaultConfig::none()
        },
        max_actions_per_actor: 1,
    };
    let responses = SimIntegrationResponses::new().on_trigger("Job", "integration", "Callback");

    let mut scripted = SimActorSystem::new(config.clone());
    scripted.set_execution_budgets(1, 1);
    scripted.register_actor("Job:1", Box::new(CallbackFailureHandler::new()));
    scripted.set_integration_responses(responses.clone());
    let error = scripted.step("Job:1", "Start", "{}").unwrap_err();
    assert!(error.contains("callback rejected"));
    assert_eq!(scripted.execution_errors().len(), 1);
    let transitions_after_failure = scripted.total_transitions;
    let retry_error = scripted.step("Job:1", "Complete", "{}").unwrap_err();
    assert!(retry_error.contains("simulation run is invalid"));
    assert_eq!(scripted.total_transitions, transitions_after_failure);
    let tick_after_failure = scripted.clock.tick();
    let messages_after_failure = scripted.total_messages;
    let resumed = scripted.run_random();
    assert!(!resumed.all_invariants_held);
    assert_eq!(scripted.clock.tick(), tick_after_failure);
    assert_eq!(scripted.total_transitions, transitions_after_failure);
    assert_eq!(scripted.total_messages, messages_after_failure);

    let mut random = SimActorSystem::new(config);
    random.set_execution_budgets(1, 1);
    random.register_actor("Job:1", Box::new(CallbackFailureHandler::new()));
    random.set_integration_responses(responses);
    let result = random.run_random();
    assert!(!result.all_invariants_held);
    assert_eq!(
        result.messages, 1,
        "the delayed action owns one reservation"
    );
    assert_eq!(random.execution_errors().len(), 1);
    assert!(
        random.execution_errors()[0]
            .description
            .contains("callback rejected")
    );
    let tick_after_random_failure = random.clock.tick();
    let resumed_random = random.run_random();
    assert_eq!(random.clock.tick(), tick_after_random_failure);
    assert_eq!(resumed_random.transitions, result.transitions);
    assert_eq!(resumed_random.messages, result.messages);
}

#[test]
fn final_tick_drains_every_due_message_when_batch_exceeds_budget() {
    let config = SimActorSystemConfig {
        seed: 3,
        max_ticks: 2,
        faults: FaultConfig {
            message_delay_prob: 1.0,
            max_delay_ticks: 2,
            ..FaultConfig::none()
        },
        max_actions_per_actor: 2,
    };
    let mut sim = SimActorSystem::new(config);
    sim.set_execution_budgets(1, 1);
    sim.register_actor("Job:1", Box::new(CallbackFailureHandler::new()));

    let result = sim.run_random();

    assert_eq!(result.dropped, 0);
    assert_eq!(result.messages, 2);
    assert_eq!(
        result.transitions, 2,
        "every message due on the final tick must leave scheduler ownership"
    );
}

#[test]
fn final_tick_batches_share_one_reaction_budget() {
    let config = SimActorSystemConfig {
        seed: 3,
        max_ticks: 2,
        faults: FaultConfig {
            message_delay_prob: 1.0,
            max_delay_ticks: 2,
            ..FaultConfig::none()
        },
        max_actions_per_actor: 2,
    };
    let mut sim = SimActorSystem::new(config);
    sim.set_execution_budgets(1, 1);
    sim.register_actor("Job:1", Box::new(CallbackFailureHandler::new()));
    sim.set_integration_responses(SimIntegrationResponses::new().on_trigger(
        "Job",
        "integration",
        "Complete",
    ));

    let result = sim.run_random();

    assert!(!result.all_invariants_held);
    assert_eq!(result.messages, 2);
    assert_eq!(result.transitions, 3);
    assert_eq!(sim.execution_errors().len(), 1);
    assert!(
        sim.execution_errors()[0]
            .description
            .contains("budget exhausted after 1 reactions")
    );
}

#[test]
fn callback_cascade_fails_when_reaction_budget_is_exhausted() {
    let config = SimActorSystemConfig::default();
    let mut sim = SimActorSystem::new(config);
    sim.set_execution_budgets(1_024, 2);
    sim.register_actor("Job:1", Box::new(CallbackFailureHandler::new()));
    sim.set_integration_responses(SimIntegrationResponses::new().on_trigger(
        "Job",
        "integration",
        "Loop",
    ));

    let error = sim.step("Job:1", "Start", "{}").unwrap_err();
    assert!(error.contains("budget exhausted after 2 reactions"));
    assert_eq!(sim.execution_errors().len(), 1);
    let transitions_after_failure = sim.total_transitions;
    let retry_error = sim.step("Job:1", "Complete", "{}").unwrap_err();
    assert!(retry_error.contains("simulation run is invalid"));
    assert_eq!(sim.total_transitions, transitions_after_failure);
    let tick_after_failure = sim.clock.tick();
    let messages_after_failure = sim.total_messages;
    let resumed = sim.run_random();
    assert!(!resumed.all_invariants_held);
    assert_eq!(sim.clock.tick(), tick_after_failure);
    assert_eq!(sim.total_transitions, transitions_after_failure);
    assert_eq!(sim.total_messages, messages_after_failure);
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
