//! Deterministic actor fidelity tests for recovery, messaging, time, and invariants.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{
    FaultConfig, SimActorHandler, SimActorSystem, SimActorSystemConfig,
};
use temper_server::entity_actor::sim_handler::EntityActorHandler;

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn order_handler(id: &str) -> EntityActorHandler {
    EntityActorHandler::new(
        "Order",
        id,
        Arc::new(TransitionTable::from_ioa_source(ORDER_IOA)),
    )
    .with_ioa_invariants(ORDER_IOA)
}

#[test]
fn restart_reconstructs_state_and_sequence_from_the_journal() {
    let mut sim = SimActorSystem::new(SimActorSystemConfig {
        seed: 123,
        ..Default::default()
    });
    sim.register_actor("Order:one", Box::new(order_handler("one")));

    sim.step(
        "Order:one",
        "AddItem",
        r#"{"ProductId":"product-1","Quantity":1}"#,
    )
    .expect("add item");
    sim.step(
        "Order:one",
        "SubmitOrder",
        r#"{"ShippingAddressId":"address-1","PaymentMethod":"card"}"#,
    )
    .expect("submit order");
    let events_before = sim.events_json("Order:one");
    assert_eq!(sim.event_sequence("Order:one"), 2);

    sim.crash_and_restart_actor("Order:one");

    assert_eq!(sim.status("Order:one"), "Submitted");
    assert_eq!(sim.event_sequence("Order:one"), 2);
    assert_eq!(sim.events_json("Order:one"), events_before);
    sim.step("Order:one", "ConfirmOrder", "{}")
        .expect("reconstructed actor accepts the next action");
    assert_eq!(sim.event_sequence("Order:one"), 3);
}

#[test]
fn restart_reconstructs_field_updates_instead_of_their_event_envelope() {
    let mut handler = order_handler("fields");
    handler.init().expect("initialize actor");
    assert!(handler.update_fields(serde_json::json!({"CustomerName": "Ada"}), false, Some(0),));
    assert_eq!(handler.string_value("CustomerName").as_deref(), Some("Ada"));

    handler.restart().expect("reconstruct actor");

    assert_eq!(handler.event_sequence(), 1);
    assert_eq!(handler.string_value("CustomerName").as_deref(), Some("Ada"));
    assert_eq!(handler.string_value("fields"), None);
}

#[test]
fn actor_to_actor_messages_cross_the_heavy_fault_scheduler() {
    let mut aggregate_delivered = 0_u64;
    let mut aggregate_dropped = 0_usize;

    for seed in 0..64 {
        let mut sim = SimActorSystem::new(SimActorSystemConfig {
            seed,
            max_ticks: 256,
            faults: FaultConfig::heavy(),
            max_actions_per_actor: 128,
        });
        sim.register_actor("Order:source", Box::new(order_handler("source")));
        sim.register_actor("Order:target", Box::new(order_handler("target")));

        for _ in 0..32 {
            sim.send_actor_message(
                "Order:source",
                "Order:target",
                "AddItem",
                r#"{"ProductId":"product-1","Quantity":1}"#,
            );
        }
        sim.run_queued(256);
        aggregate_delivered += sim.event_sequence("Order:target");
        aggregate_dropped += sim.dropped_messages();
        assert!(!sim.has_violations(), "seed {seed}: {:?}", sim.violations());

        let events = sim.events_json("Order:target");
        let sequence = sim.event_sequence("Order:target");
        sim.crash_and_restart_actor("Order:target");
        assert_eq!(sim.event_sequence("Order:target"), sequence);
        assert_eq!(sim.events_json("Order:target"), events);
    }

    assert!(
        aggregate_delivered > 0,
        "heavy faults delivered no cross-entity work"
    );
    assert!(
        aggregate_dropped > 0,
        "heavy faults dropped no cross-entity work"
    );
}

#[test]
fn clock_skew_and_forward_jump_are_deterministic_and_scoped() {
    fn trial() -> serde_json::Value {
        let mut sim = SimActorSystem::new(SimActorSystemConfig {
            seed: 77,
            ..Default::default()
        });
        sim.register_actor("Order:clock", Box::new(order_handler("clock")));
        sim.set_actor_clock_skew_ms("Order:clock", 5_000);
        sim.step(
            "Order:clock",
            "AddItem",
            r#"{"ProductId":"product-1","Quantity":1}"#,
        )
        .expect("skewed add");
        sim.jump_clock_by(100);
        sim.set_actor_clock_skew_ms("Order:clock", -2_500);
        sim.step("Order:clock", "RemoveItem", r#"{"ItemId":"item-1"}"#)
            .expect("post-jump remove");
        sim.events_json("Order:clock")
    }

    let first = trial();
    let second = trial();
    assert_eq!(
        first, second,
        "clock anomaly trial must replay byte-for-byte"
    );

    let events = first.as_array().expect("event array");
    let first_ts: DateTime<Utc> =
        serde_json::from_value(events[0]["timestamp"].clone()).expect("first timestamp");
    let second_ts: DateTime<Utc> =
        serde_json::from_value(events[1]["timestamp"].clone()).expect("second timestamp");
    assert!(
        second_ts > first_ts,
        "forward jump must dominate negative skew"
    );
}

#[test]
fn missing_counter_evidence_fails_the_declared_invariant() {
    const SPEC: &str = r#"
[automaton]
name = "CounterProof"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "attempts"
type = "counter"
initial = "0"

[[action]]
name = "Finish"
from = ["Open"]
to = "Done"

[[invariant]]
name = "DoneRequiresAttempt"
when = ["Done"]
assert = "attempts > 0"
"#;
    let mut sim = SimActorSystem::new(SimActorSystemConfig::default());
    let handler = EntityActorHandler::new(
        "CounterProof",
        "proof",
        Arc::new(TransitionTable::from_ioa_source(SPEC)),
    )
    .with_ioa_invariants(SPEC);
    sim.register_actor("proof", Box::new(handler));
    sim.step("proof", "Finish", "{}")
        .expect("transition executes");
    assert!(
        sim.has_violations(),
        "zero counter must not pass attempts > 0"
    );
}

#[test]
fn terminal_invariant_checks_the_actual_enabled_action_set() {
    const SPEC: &str = r#"
[automaton]
name = "TerminalProof"
states = ["Open", "Done"]
initial = "Open"

[[action]]
name = "Finish"
from = ["Open"]
to = "Done"

[[action]]
name = "Reopen"
from = ["Done"]
to = "Open"

[[invariant]]
name = "DoneIsTerminal"
when = ["Done"]
assert = "no_further_transitions"
"#;
    let mut sim = SimActorSystem::new(SimActorSystemConfig::default());
    let handler = EntityActorHandler::new(
        "TerminalProof",
        "proof",
        Arc::new(TransitionTable::from_ioa_source(SPEC)),
    )
    .with_ioa_invariants(SPEC);
    sim.register_actor("proof", Box::new(handler));
    sim.step("proof", "Finish", "{}")
        .expect("transition executes");
    assert!(
        sim.has_violations(),
        "enabled Reopen must violate terminality"
    );
}
