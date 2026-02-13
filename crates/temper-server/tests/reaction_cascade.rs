//! Integration test: cross-entity reaction cascade via SimReactionSystem.
//!
//! Simulates an e-commerce flow: Order → Payment choreography.
//! When an Order reaches "Confirmed" via ConfirmOrder, a reaction rule
//! automatically triggers AuthorizePayment on the associated Payment entity.

use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{install_deterministic_context, SimActorSystemConfig, FaultConfig};
use temper_server::reaction::registry::{ReactionRegistry, parse_reactions};
use temper_server::reaction::sim_dispatcher::SimReactionSystem;
use temper_server::reaction::types::{
    ReactionRule, ReactionTrigger, ReactionTarget, TargetResolver,
};

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

/// Minimal Payment spec for testing the cascade.
const PAYMENT_IOA: &str = r#"
[automaton]
name = "Payment"
initial = "Pending"
states = ["Pending", "Authorized", "Captured", "Failed"]

[[action]]
name = "AuthorizePayment"
from = ["Pending"]
to = "Authorized"
kind = "internal"

[[action]]
name = "CapturePayment"
from = ["Authorized"]
to = "Captured"
kind = "internal"

[[action]]
name = "FailPayment"
from = ["Pending", "Authorized"]
to = "Failed"
kind = "internal"
"#;

fn order_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(ORDER_IOA))
}

fn payment_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(PAYMENT_IOA))
}

fn ecommerce_registry() -> ReactionRegistry {
    let mut reg = ReactionRegistry::new();
    reg.register_tenant_rules("shop", vec![
        ReactionRule {
            name: "order_confirmed_triggers_payment".to_string(),
            when: ReactionTrigger {
                entity_type: "Order".to_string(),
                action: Some("ConfirmOrder".to_string()),
                to_state: Some("Confirmed".to_string()),
            },
            then: ReactionTarget {
                entity_type: "Payment".to_string(),
                action: "AuthorizePayment".to_string(),
                params: serde_json::json!({}),
            },
            resolve_target: TargetResolver::SameId,
        },
    ]);
    reg
}

fn sim_config() -> SimActorSystemConfig {
    SimActorSystemConfig {
        seed: 42,
        max_ticks: 100,
        faults: FaultConfig::none(),
        max_actions_per_actor: 20,
    }
}

// =========================================================================
// E-commerce cascade test
// =========================================================================

#[test]
fn order_confirm_triggers_payment_authorize() {
    let (_guard, clock, _id_gen) = install_deterministic_context(42);

    let mut sys = SimReactionSystem::new(sim_config(), ecommerce_registry(), "shop");

    // Register Order and Payment actors with same entity ID ("e1")
    sys.register_entity("order-e1", "Order", "e1", order_table());
    sys.register_entity("payment-e1", "Payment", "e1", payment_table());

    // Drive Order: AddItem → SubmitOrder → ConfirmOrder
    clock.advance();
    sys.step("order-e1", "AddItem", r#"{"ProductId":"laptop"}"#).unwrap();
    sys.assert_status("order-e1", "Draft");

    clock.advance();
    sys.step("order-e1", "SubmitOrder", "{}").unwrap();
    sys.assert_status("order-e1", "Submitted");

    clock.advance();
    // This should trigger the reaction: Payment → AuthorizePayment
    sys.step("order-e1", "ConfirmOrder", "{}").unwrap();
    sys.assert_status("order-e1", "Confirmed");

    // Payment should have been automatically authorized by the reaction
    sys.assert_status("payment-e1", "Authorized");

    // Verify reaction results
    let results = sys.last_results();
    assert_eq!(results.len(), 1);
    assert!(results[0].success);
    assert_eq!(results[0].rule_name, "order_confirmed_triggers_payment");
    assert_eq!(results[0].target_status.as_deref(), Some("Authorized"));
    assert_eq!(results[0].depth, 0);
}

// =========================================================================
// No infinite loop test
// =========================================================================

#[test]
fn cascade_stops_without_infinite_loop() {
    let (_guard, clock, _id_gen) = install_deterministic_context(99);

    let mut sys = SimReactionSystem::new(sim_config(), ecommerce_registry(), "shop");
    sys.register_entity("order-e2", "Order", "e2", order_table());
    sys.register_entity("payment-e2", "Payment", "e2", payment_table());

    clock.advance();
    sys.step("order-e2", "AddItem", "{}").unwrap();
    clock.advance();
    sys.step("order-e2", "SubmitOrder", "{}").unwrap();
    clock.advance();
    sys.step("order-e2", "ConfirmOrder", "{}").unwrap();

    // If cascade didn't stop, we'd never reach here
    sys.assert_status("order-e2", "Confirmed");
    sys.assert_status("payment-e2", "Authorized");

    // Only 1 reaction fired (no infinite loop)
    assert_eq!(sys.last_results().len(), 1);
}

// =========================================================================
// No reaction when trigger doesn't match
// =========================================================================

#[test]
fn no_reaction_when_action_doesnt_match() {
    let (_guard, clock, _id_gen) = install_deterministic_context(55);

    let mut sys = SimReactionSystem::new(sim_config(), ecommerce_registry(), "shop");
    sys.register_entity("order-e3", "Order", "e3", order_table());
    sys.register_entity("payment-e3", "Payment", "e3", payment_table());

    // AddItem should NOT trigger any reaction
    clock.advance();
    sys.step("order-e3", "AddItem", r#"{"ProductId":"phone"}"#).unwrap();
    assert!(sys.last_results().is_empty());

    // SubmitOrder should NOT trigger either (only ConfirmOrder does)
    clock.advance();
    sys.step("order-e3", "SubmitOrder", "{}").unwrap();
    assert!(sys.last_results().is_empty());
}

// =========================================================================
// Field-based target resolution
// =========================================================================

#[test]
fn field_based_target_resolution() {
    let (_guard, clock, _id_gen) = install_deterministic_context(77);

    // Rule resolves payment ID from a field on the Order
    let mut reg = ReactionRegistry::new();
    reg.register_tenant_rules("shop2", vec![
        ReactionRule {
            name: "order_to_payment_via_field".to_string(),
            when: ReactionTrigger {
                entity_type: "Order".to_string(),
                action: Some("ConfirmOrder".to_string()),
                to_state: Some("Confirmed".to_string()),
            },
            then: ReactionTarget {
                entity_type: "Payment".to_string(),
                action: "AuthorizePayment".to_string(),
                params: serde_json::json!({}),
            },
            resolve_target: TargetResolver::Field {
                field: "payment_id".to_string(),
            },
        },
    ]);

    let mut sys = SimReactionSystem::new(sim_config(), reg, "shop2");
    sys.register_entity("order-f1", "Order", "f1", order_table());
    sys.register_entity("payment-p99", "Payment", "p99", payment_table());

    // The order's fields won't contain "payment_id" since it's not part of
    // the IOA spec — so target resolution will fail gracefully
    clock.advance();
    sys.step("order-f1", "AddItem", "{}").unwrap();
    clock.advance();
    sys.step("order-f1", "SubmitOrder", "{}").unwrap();
    clock.advance();
    sys.step("order-f1", "ConfirmOrder", "{}").unwrap();

    // Payment should still be Pending (field not found)
    sys.assert_status("payment-p99", "Pending");
    let results = sys.last_results();
    assert_eq!(results.len(), 1);
    assert!(!results[0].success);
    assert!(results[0].error.as_ref().unwrap().contains("Could not resolve"));
}

// =========================================================================
// TOML parsing integration
// =========================================================================

#[test]
fn parse_and_register_reactions_from_toml() {
    let toml = r#"
[[reaction]]
name = "order_confirmed_triggers_payment"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
to_state = "Confirmed"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"
"#;

    let rules = parse_reactions(toml).unwrap();
    assert_eq!(rules.len(), 1);

    let mut reg = ReactionRegistry::new();
    reg.register_tenant_rules("t1", rules);

    let tenant = temper_runtime::tenant::TenantId::new("t1");
    let results = reg.lookup(&tenant, "Order", "ConfirmOrder", "Confirmed");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].then.action, "AuthorizePayment");
}

// =========================================================================
// Multi-step cascade (Order → Payment → ... stops at depth)
// =========================================================================

#[test]
fn multi_step_cascade_with_chained_reactions() {
    let (_guard, clock, _id_gen) = install_deterministic_context(123);

    // Chain: Order:ConfirmOrder → Payment:AuthorizePayment → Payment:CapturePayment
    // (second rule triggers on Payment reaching Authorized)
    let mut reg = ReactionRegistry::new();
    reg.register_tenant_rules("chain", vec![
        ReactionRule {
            name: "confirm_triggers_authorize".to_string(),
            when: ReactionTrigger {
                entity_type: "Order".to_string(),
                action: Some("ConfirmOrder".to_string()),
                to_state: Some("Confirmed".to_string()),
            },
            then: ReactionTarget {
                entity_type: "Payment".to_string(),
                action: "AuthorizePayment".to_string(),
                params: serde_json::json!({}),
            },
            resolve_target: TargetResolver::SameId,
        },
        ReactionRule {
            name: "authorize_triggers_capture".to_string(),
            when: ReactionTrigger {
                entity_type: "Payment".to_string(),
                action: Some("AuthorizePayment".to_string()),
                to_state: Some("Authorized".to_string()),
            },
            then: ReactionTarget {
                entity_type: "Payment".to_string(),
                action: "CapturePayment".to_string(),
                params: serde_json::json!({}),
            },
            resolve_target: TargetResolver::SameId,
        },
    ]);

    let mut sys = SimReactionSystem::new(sim_config(), reg, "chain");
    sys.register_entity("order-c1", "Order", "c1", order_table());
    sys.register_entity("payment-c1", "Payment", "c1", payment_table());

    clock.advance();
    sys.step("order-c1", "AddItem", "{}").unwrap();
    clock.advance();
    sys.step("order-c1", "SubmitOrder", "{}").unwrap();
    clock.advance();
    sys.step("order-c1", "ConfirmOrder", "{}").unwrap();

    // Order confirmed, Payment should be fully captured (two-step cascade)
    sys.assert_status("order-c1", "Confirmed");
    sys.assert_status("payment-c1", "Captured");

    // Two reactions should have fired
    let results = sys.last_results();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].rule_name, "confirm_triggers_authorize");
    assert_eq!(results[0].depth, 0);
    assert_eq!(results[1].rule_name, "authorize_triggers_capture");
    assert_eq!(results[1].depth, 1);
}

// =========================================================================
// No violations during cascade
// =========================================================================

#[test]
fn cascade_does_not_cause_invariant_violations() {
    let (_guard, clock, _id_gen) = install_deterministic_context(42);

    let mut sys = SimReactionSystem::new(sim_config(), ecommerce_registry(), "shop");
    sys.register_entity("order-v1", "Order", "v1", order_table());
    sys.register_entity("payment-v1", "Payment", "v1", payment_table());

    clock.advance();
    sys.step("order-v1", "AddItem", "{}").unwrap();
    clock.advance();
    sys.step("order-v1", "SubmitOrder", "{}").unwrap();
    clock.advance();
    sys.step("order-v1", "ConfirmOrder", "{}").unwrap();

    assert!(!sys.has_violations());
}
