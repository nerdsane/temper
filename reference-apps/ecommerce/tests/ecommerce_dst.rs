//! Deterministic Simulation Tests for e-commerce entities.
//!
//! These DST tests exercise the Order, Payment, and Shipment specs through
//! the SimActorSystem with:
//!
//! - **Scripted scenarios**: exact action sequences with state assertions
//! - **Random exploration**: seed-controlled random walks with fault injection
//! - **Determinism proofs**: bit-exact replay across multiple runs
//! - **Multi-entity scenarios**: Order + Payment + Shipment interacting together
//! - **Multi-seed sweeps**: bulk exploration across many seeds

use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{
    FaultConfig, SimActorSystem, SimActorSystemConfig,
};
use temper_server::entity_actor::sim_handler::EntityActorHandler;

const ORDER_IOA: &str = include_str!("../specs/order.ioa.toml");
const PAYMENT_IOA: &str = include_str!("../specs/payment.ioa.toml");
const SHIPMENT_IOA: &str = include_str!("../specs/shipment.ioa.toml");

fn order_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(ORDER_IOA))
}

fn payment_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(PAYMENT_IOA))
}

fn shipment_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(SHIPMENT_IOA))
}

// =========================================================================
// SCRIPTED SCENARIOS — Order Lifecycle
// =========================================================================

#[test]
fn scripted_order_starts_in_draft() {
    let config = SimActorSystemConfig { seed: 1, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Order", "ord-1", order_table())
        .with_ioa_invariants(ORDER_IOA);
    sim.register_actor("ord-1", Box::new(handler));

    sim.assert_status("ord-1", "Draft");
}

#[test]
fn scripted_order_add_item_then_submit() {
    let config = SimActorSystemConfig { seed: 2, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Order", "ord-1", order_table())
        .with_ioa_invariants(ORDER_IOA);
    sim.register_actor("ord-1", Box::new(handler));

    // Draft → AddItem (stays in Draft, increments item counter)
    sim.step("ord-1", "AddItem", "{}").unwrap();
    sim.assert_status("ord-1", "Draft");

    // Draft → SubmitOrder (now items > 0, guard passes)
    sim.step("ord-1", "SubmitOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Submitted");

    sim.assert_event_count("ord-1", 2);
    assert!(!sim.has_violations());
}

#[test]
fn scripted_order_full_lifecycle() {
    let config = SimActorSystemConfig { seed: 3, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Order", "ord-1", order_table())
        .with_ioa_invariants(ORDER_IOA);
    sim.register_actor("ord-1", Box::new(handler));

    // Draft → AddItem → SubmitOrder → Confirmed → Processing → Shipped → Delivered
    sim.step("ord-1", "AddItem", "{}").unwrap();
    sim.step("ord-1", "SubmitOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Submitted");

    sim.step("ord-1", "ConfirmOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Confirmed");

    sim.step("ord-1", "ProcessOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Processing");

    sim.step("ord-1", "ShipOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Shipped");

    sim.step("ord-1", "DeliverOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Delivered");

    sim.assert_event_count("ord-1", 6);
    assert!(!sim.has_violations());
}

#[test]
fn scripted_order_cancel_from_draft() {
    let config = SimActorSystemConfig { seed: 4, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Order", "ord-1", order_table())
        .with_ioa_invariants(ORDER_IOA);
    sim.register_actor("ord-1", Box::new(handler));

    sim.step("ord-1", "CancelOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Cancelled");
    assert!(!sim.has_violations());
}

#[test]
fn scripted_order_cancel_from_submitted() {
    let config = SimActorSystemConfig { seed: 5, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Order", "ord-1", order_table())
        .with_ioa_invariants(ORDER_IOA);
    sim.register_actor("ord-1", Box::new(handler));

    sim.step("ord-1", "AddItem", "{}").unwrap();
    sim.step("ord-1", "SubmitOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Submitted");

    sim.step("ord-1", "CancelOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Cancelled");
    assert!(!sim.has_violations());
}

#[test]
fn scripted_order_cannot_submit_empty() {
    let config = SimActorSystemConfig { seed: 6, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Order", "ord-1", order_table())
        .with_ioa_invariants(ORDER_IOA);
    sim.register_actor("ord-1", Box::new(handler));

    // SubmitOrder from Draft without items should fail (guard: items > 0)
    let result = sim.step("ord-1", "SubmitOrder", "{}");
    assert!(result.is_err(), "SubmitOrder should fail with 0 items");
    sim.assert_status("ord-1", "Draft");
}

#[test]
fn scripted_order_return_flow() {
    let config = SimActorSystemConfig { seed: 7, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Order", "ord-1", order_table())
        .with_ioa_invariants(ORDER_IOA);
    sim.register_actor("ord-1", Box::new(handler));

    // Full lifecycle to Delivered
    sim.step("ord-1", "AddItem", "{}").unwrap();
    sim.step("ord-1", "SubmitOrder", "{}").unwrap();
    sim.step("ord-1", "ConfirmOrder", "{}").unwrap();
    sim.step("ord-1", "ProcessOrder", "{}").unwrap();
    sim.step("ord-1", "ShipOrder", "{}").unwrap();
    sim.step("ord-1", "DeliverOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Delivered");

    // Delivered → InitiateReturn → CompleteReturn → RefundOrder
    sim.step("ord-1", "InitiateReturn", "{}").unwrap();
    sim.assert_status("ord-1", "ReturnRequested");

    sim.step("ord-1", "CompleteReturn", "{}").unwrap();
    sim.assert_status("ord-1", "Returned");

    sim.step("ord-1", "RefundOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Refunded");

    assert!(!sim.has_violations());
}

// =========================================================================
// SCRIPTED SCENARIOS — Payment Lifecycle
// =========================================================================

#[test]
fn scripted_payment_full_lifecycle() {
    let config = SimActorSystemConfig { seed: 10, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Payment", "pay-1", payment_table())
        .with_ioa_invariants(PAYMENT_IOA);
    sim.register_actor("pay-1", Box::new(handler));

    sim.assert_status("pay-1", "Pending");

    sim.step("pay-1", "AuthorizePayment", "{}").unwrap();
    sim.assert_status("pay-1", "Authorized");

    sim.step("pay-1", "CapturePayment", "{}").unwrap();
    sim.assert_status("pay-1", "Captured");

    sim.assert_event_count("pay-1", 2);
    assert!(!sim.has_violations());
}

#[test]
fn scripted_payment_failure() {
    let config = SimActorSystemConfig { seed: 11, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Payment", "pay-1", payment_table())
        .with_ioa_invariants(PAYMENT_IOA);
    sim.register_actor("pay-1", Box::new(handler));

    sim.step("pay-1", "FailPayment", "{}").unwrap();
    sim.assert_status("pay-1", "Failed");

    // Failed is terminal — no further transitions allowed
    let result = sim.step("pay-1", "AuthorizePayment", "{}");
    assert!(result.is_err(), "AuthorizePayment should fail from Failed state");
    sim.assert_status("pay-1", "Failed");

    assert!(!sim.has_violations());
}

#[test]
fn scripted_payment_refund() {
    let config = SimActorSystemConfig { seed: 12, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Payment", "pay-1", payment_table())
        .with_ioa_invariants(PAYMENT_IOA);
    sim.register_actor("pay-1", Box::new(handler));

    sim.step("pay-1", "AuthorizePayment", "{}").unwrap();
    sim.step("pay-1", "CapturePayment", "{}").unwrap();
    sim.step("pay-1", "RefundPayment", "{}").unwrap();
    sim.assert_status("pay-1", "Refunded");

    // Refunded is terminal
    let result = sim.step("pay-1", "CapturePayment", "{}");
    assert!(result.is_err(), "CapturePayment should fail from Refunded state");

    assert!(!sim.has_violations());
}

// =========================================================================
// SCRIPTED SCENARIOS — Shipment Lifecycle
// =========================================================================

#[test]
fn scripted_shipment_full_delivery() {
    let config = SimActorSystemConfig { seed: 20, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Shipment", "ship-1", shipment_table())
        .with_ioa_invariants(SHIPMENT_IOA);
    sim.register_actor("ship-1", Box::new(handler));

    sim.assert_status("ship-1", "Created");

    sim.step("ship-1", "ShipOrder", "{}").unwrap();
    sim.assert_status("ship-1", "PickedUp");

    sim.step("ship-1", "MarkInTransit", "{}").unwrap();
    sim.assert_status("ship-1", "InTransit");

    sim.step("ship-1", "MarkOutForDelivery", "{}").unwrap();
    sim.assert_status("ship-1", "OutForDelivery");

    sim.step("ship-1", "DeliverShipment", "{}").unwrap();
    sim.assert_status("ship-1", "Delivered");

    sim.assert_event_count("ship-1", 4);
    assert!(!sim.has_violations());
}

#[test]
fn scripted_shipment_failure_and_return() {
    let config = SimActorSystemConfig { seed: 21, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    let handler = EntityActorHandler::new("Shipment", "ship-1", shipment_table())
        .with_ioa_invariants(SHIPMENT_IOA);
    sim.register_actor("ship-1", Box::new(handler));

    sim.step("ship-1", "ShipOrder", "{}").unwrap();
    sim.step("ship-1", "MarkInTransit", "{}").unwrap();
    sim.assert_status("ship-1", "InTransit");

    sim.step("ship-1", "FailDelivery", "{}").unwrap();
    sim.assert_status("ship-1", "Failed");

    sim.step("ship-1", "ReturnShipment", "{}").unwrap();
    sim.assert_status("ship-1", "Returned");

    // Returned is terminal
    let result = sim.step("ship-1", "ShipOrder", "{}");
    assert!(result.is_err(), "ShipOrder should fail from Returned state");

    assert!(!sim.has_violations());
}

// =========================================================================
// MULTI-ENTITY SCENARIO — Full e-commerce flow
// =========================================================================

#[test]
fn scripted_ecommerce_full_scenario() {
    let config = SimActorSystemConfig { seed: 100, ..Default::default() };
    let mut sim = SimActorSystem::new(config);

    // Register all three entity types
    let order = EntityActorHandler::new("Order", "ord-1", order_table())
        .with_ioa_invariants(ORDER_IOA);
    let payment = EntityActorHandler::new("Payment", "pay-1", payment_table())
        .with_ioa_invariants(PAYMENT_IOA);
    let shipment = EntityActorHandler::new("Shipment", "ship-1", shipment_table())
        .with_ioa_invariants(SHIPMENT_IOA);

    sim.register_actor("ord-1", Box::new(order));
    sim.register_actor("pay-1", Box::new(payment));
    sim.register_actor("ship-1", Box::new(shipment));

    // 1. Customer adds item and submits order
    sim.step("ord-1", "AddItem", "{}").unwrap();
    sim.step("ord-1", "SubmitOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Submitted");

    // 2. Payment authorized and captured
    sim.step("pay-1", "AuthorizePayment", "{}").unwrap();
    sim.step("pay-1", "CapturePayment", "{}").unwrap();
    sim.assert_status("pay-1", "Captured");

    // 3. Order confirmed and processed
    sim.step("ord-1", "ConfirmOrder", "{}").unwrap();
    sim.step("ord-1", "ProcessOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Processing");

    // 4. Shipment created and delivered
    sim.step("ship-1", "ShipOrder", "{}").unwrap();
    sim.step("ship-1", "MarkInTransit", "{}").unwrap();
    sim.step("ship-1", "MarkOutForDelivery", "{}").unwrap();
    sim.step("ship-1", "DeliverShipment", "{}").unwrap();
    sim.assert_status("ship-1", "Delivered");

    // 5. Order shipped and delivered
    sim.step("ord-1", "ShipOrder", "{}").unwrap();
    sim.step("ord-1", "DeliverOrder", "{}").unwrap();
    sim.assert_status("ord-1", "Delivered");

    // All actors progressed without violations
    assert!(!sim.has_violations(), "violations: {:?}", sim.violations());
}

// =========================================================================
// RANDOM EXPLORATION — No-fault
// =========================================================================

#[test]
fn random_order_no_faults() {
    let config = SimActorSystemConfig {
        seed: 42,
        max_ticks: 200,
        faults: FaultConfig::none(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    for i in 0..3 {
        let handler = EntityActorHandler::new("Order", &format!("ord-{i}"), order_table())
            .with_ioa_invariants(ORDER_IOA);
        sim.register_actor(&format!("ord-{i}"), Box::new(handler));
    }

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Random exploration found invariant violations: {:?}",
        result.violations
    );
    assert!(result.transitions > 0, "Should have at least one transition");
}

#[test]
fn random_all_entities_light_faults() {
    let config = SimActorSystemConfig {
        seed: 99,
        max_ticks: 300,
        faults: FaultConfig::light(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    sim.register_actor("ord-1", Box::new(
        EntityActorHandler::new("Order", "ord-1", order_table())
            .with_ioa_invariants(ORDER_IOA)));
    sim.register_actor("pay-1", Box::new(
        EntityActorHandler::new("Payment", "pay-1", payment_table())
            .with_ioa_invariants(PAYMENT_IOA)));
    sim.register_actor("ship-1", Box::new(
        EntityActorHandler::new("Shipment", "ship-1", shipment_table())
            .with_ioa_invariants(SHIPMENT_IOA)));

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Light faults should not break invariants: {:?}",
        result.violations
    );
}

#[test]
fn random_all_entities_heavy_faults() {
    let config = SimActorSystemConfig {
        seed: 1337,
        max_ticks: 500,
        faults: FaultConfig::heavy(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    sim.register_actor("ord-1", Box::new(
        EntityActorHandler::new("Order", "ord-1", order_table())
            .with_ioa_invariants(ORDER_IOA)));
    sim.register_actor("pay-1", Box::new(
        EntityActorHandler::new("Payment", "pay-1", payment_table())
            .with_ioa_invariants(PAYMENT_IOA)));
    sim.register_actor("ship-1", Box::new(
        EntityActorHandler::new("Shipment", "ship-1", shipment_table())
            .with_ioa_invariants(SHIPMENT_IOA)));

    let result = sim.run_random();
    assert!(
        result.all_invariants_held,
        "Even heavy faults should not break invariants: {:?}",
        result.violations
    );
}

// =========================================================================
// DETERMINISM PROOFS — same seed = bit-exact same outcome
// =========================================================================

fn run_determinism_trial(seed: u64) -> Vec<(String, String, usize, usize)> {
    let config = SimActorSystemConfig {
        seed,
        max_ticks: 300,
        faults: FaultConfig::light(),
        max_actions_per_actor: 30,
    };
    let mut sim = SimActorSystem::new(config);

    sim.register_actor("ord-1", Box::new(
        EntityActorHandler::new("Order", "ord-1", order_table())
            .with_ioa_invariants(ORDER_IOA)));
    sim.register_actor("pay-1", Box::new(
        EntityActorHandler::new("Payment", "pay-1", payment_table())
            .with_ioa_invariants(PAYMENT_IOA)));
    sim.register_actor("ship-1", Box::new(
        EntityActorHandler::new("Shipment", "ship-1", shipment_table())
            .with_ioa_invariants(SHIPMENT_IOA)));

    let result = sim.run_random();
    assert!(result.all_invariants_held);
    result.actor_states
}

#[test]
fn determinism_proof_seed_42() {
    let reference = run_determinism_trial(42);
    for run in 1..10 {
        let trial = run_determinism_trial(42);
        assert_eq!(
            reference, trial,
            "Determinism violation on run {run}: seed 42 must produce identical results"
        );
    }
}

#[test]
fn determinism_proof_seed_1337() {
    let reference = run_determinism_trial(1337);
    for run in 1..10 {
        let trial = run_determinism_trial(1337);
        assert_eq!(
            reference, trial,
            "Determinism violation on run {run}: seed 1337 must produce identical results"
        );
    }
}

// =========================================================================
// MULTI-SEED SWEEP — bulk exploration
// =========================================================================

#[test]
fn multi_seed_sweep_all_entities() {
    for seed in 0..20 {
        let config = SimActorSystemConfig {
            seed,
            max_ticks: 100,
            faults: FaultConfig::light(),
            max_actions_per_actor: 20,
        };
        let mut sim = SimActorSystem::new(config);

        sim.register_actor("ord", Box::new(
            EntityActorHandler::new("Order", "ord", order_table())
                .with_ioa_invariants(ORDER_IOA)));
        sim.register_actor("pay", Box::new(
            EntityActorHandler::new("Payment", "pay", payment_table())
                .with_ioa_invariants(PAYMENT_IOA)));
        sim.register_actor("ship", Box::new(
            EntityActorHandler::new("Shipment", "ship", shipment_table())
                .with_ioa_invariants(SHIPMENT_IOA)));

        let result = sim.run_random();
        assert!(
            result.all_invariants_held,
            "Seed {seed} found invariant violations: {:?}",
            result.violations
        );
    }
}
