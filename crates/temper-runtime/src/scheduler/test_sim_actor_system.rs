use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::SimActorState;
use super::*;

struct CountingHandler {
    applications: Arc<AtomicUsize>,
    emit_trigger: bool,
    fired: Cell<bool>,
}

impl SimActorHandler for CountingHandler {
    fn init(&mut self) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({"status": "Ready"}))
    }

    fn handle_message(&mut self, action: &str, _params: &str) -> Result<serde_json::Value, String> {
        if action == "AlwaysFails" || action == "Rejected" {
            return Err("action rejected".to_string());
        }
        self.applications.fetch_add(1, Ordering::SeqCst);
        self.fired.set(self.emit_trigger);
        Ok(serde_json::json!({"status": "Ready"}))
    }

    fn current_status(&self) -> String {
        "Ready".to_string()
    }

    fn current_item_count(&self) -> usize {
        0
    }

    fn event_count(&self) -> usize {
        self.applications.load(Ordering::SeqCst)
    }

    fn valid_actions(&self) -> Vec<String> {
        vec!["Step".to_string()]
    }

    fn events_json(&self) -> serde_json::Value {
        serde_json::json!([])
    }

    fn pending_callbacks(&self) -> Vec<String> {
        if self.fired.take() {
            vec!["boom_trigger".to_string()]
        } else {
            Vec::new()
        }
    }
}

fn counting_system(seed: u64, faults: FaultConfig) -> (SimActorSystem, Arc<AtomicUsize>) {
    let applications = Arc::new(AtomicUsize::new(0));
    let mut system = SimActorSystem::new(SimActorSystemConfig {
        seed,
        max_ticks: 200,
        faults,
        max_actions_per_actor: 30,
    });
    system.register_actor(
        "counter",
        Box::new(CountingHandler {
            applications: applications.clone(),
            emit_trigger: false,
            fired: Cell::new(false),
        }),
    );
    (system, applications)
}

#[test]
fn processed_messages_leave_no_mailbox_owner() {
    let (mut system, applications) = counting_system(7, FaultConfig::none());
    let result = system.run_random();

    assert_eq!(applications.load(Ordering::SeqCst) as u64, result.messages);
    assert_eq!(system.scheduler.mailbox_depth("counter"), 0);
    assert!(system.scheduler.is_quiescent());
}

#[test]
fn delayed_tail_messages_are_applied_exactly_once() {
    for seed in 0..50 {
        let faults = FaultConfig {
            message_delay_prob: 0.5,
            max_delay_ticks: 8,
            message_drop_prob: 0.0,
            actor_crash_prob: 0.0,
            actor_restart_prob: 0.0,
        };
        let (mut system, applications) = counting_system(seed, faults);
        let result = system.run_random();
        assert_eq!(
            applications.load(Ordering::SeqCst) as u64,
            result.messages,
            "seed {seed} lost or duplicated a scheduled message"
        );
        assert!(system.scheduler.is_quiescent(), "seed {seed}");
    }
}

#[test]
fn post_delivery_crash_happens_after_message_application() {
    let faults = FaultConfig {
        actor_crash_prob: 1.0,
        ..FaultConfig::none()
    };
    let (mut system, applications) = counting_system(19, faults);
    system.scheduler.send("driver", "counter", "Step", "{}");

    system.run_queued(1);

    assert_eq!(applications.load(Ordering::SeqCst), 1);
    assert_eq!(
        system.scheduler.actor_state("counter"),
        Some(&SimActorState::Crashed)
    );
    assert_eq!(system.scheduler.mailbox_depth("counter"), 0);
}

#[test]
fn crashed_actor_restarts_on_a_later_tick_and_continues() {
    let faults = FaultConfig {
        actor_restart_prob: 1.0,
        ..FaultConfig::none()
    };
    let (mut system, applications) = counting_system(23, faults);
    system.scheduler.crash_actor("counter");

    let result = system.run_random();

    assert!(applications.load(Ordering::SeqCst) > 0);
    assert!(result.transitions > 0);
    assert_eq!(
        system.scheduler.actor_state("counter"),
        Some(&SimActorState::Running)
    );
}

#[test]
fn scheduled_handler_rejection_is_a_violation() {
    let (mut system, _) = counting_system(13, FaultConfig::none());
    system.scheduler.send("driver", "counter", "Rejected", "{}");
    system.run_queued(4);

    assert!(
        system
            .violations
            .iter()
            .any(|violation| violation.description.contains("scheduled handler rejected"))
    );
}

#[test]
fn integration_callback_rejection_is_a_violation() {
    let applications = Arc::new(AtomicUsize::new(0));
    let mut system = SimActorSystem::new(SimActorSystemConfig {
        seed: 11,
        max_ticks: 50,
        faults: FaultConfig::none(),
        max_actions_per_actor: 3,
    });
    system.set_integration_responses(SimIntegrationResponses::new().on_trigger(
        "counter",
        "boom_trigger",
        "AlwaysFails",
    ));
    system.register_actor(
        "counter",
        Box::new(CountingHandler {
            applications,
            emit_trigger: true,
            fired: Cell::new(false),
        }),
    );

    let result = system.run_random();
    assert!(!result.all_invariants_held);
    assert!(result.violations.iter().any(|violation| {
        violation
            .description
            .contains("integration callback rejected")
    }));
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
