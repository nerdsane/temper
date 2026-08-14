//! Integration test: cross-entity reaction cascade via SimReactionSystem.
//!
//! Simulates an e-commerce flow: Order → Payment choreography.
//! When an Order reaches "Confirmed" via ConfirmOrder, a reaction rule
//! automatically triggers AuthorizePayment on the associated Payment entity.

use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{FaultConfig, SimActorSystemConfig, install_deterministic_context};
use temper_server::trigger::registry::{ReactionRegistry, parse_reactions};
use temper_server::trigger::sim_dispatcher::SimReactionSystem;
use temper_server::trigger::types::{
    ReactionGuard, ReactionRule, ReactionTarget, ReactionTrigger, TargetResolver,
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
    reg.register_tenant_rules(
        "shop",
        vec![ReactionRule {
            name: "order_confirmed_triggers_payment".to_string(),
            when: ReactionTrigger {
                entity_type: "Order".to_string(),
                action: Some("ConfirmOrder".to_string()),
                to_state: Some("Confirmed".to_string()),
                guard: None,
            },
            then: ReactionTarget {
                entity_type: "Payment".to_string(),
                action: "AuthorizePayment".to_string(),
                params: serde_json::json!({}),
                params_from: std::collections::BTreeMap::new(),
            },
            resolve_target: TargetResolver::SameId,
            principal: None,
            drop_ok: false,
        }],
    );
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
    sys.step("order-e1", "AddItem", r#"{"ProductId":"laptop"}"#)
        .unwrap();
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
    sys.step("order-e3", "AddItem", r#"{"ProductId":"phone"}"#)
        .unwrap();
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
    reg.register_tenant_rules(
        "shop2",
        vec![ReactionRule {
            name: "order_to_payment_via_field".to_string(),
            when: ReactionTrigger {
                entity_type: "Order".to_string(),
                action: Some("ConfirmOrder".to_string()),
                to_state: Some("Confirmed".to_string()),
                guard: None,
            },
            then: ReactionTarget {
                entity_type: "Payment".to_string(),
                action: "AuthorizePayment".to_string(),
                params: serde_json::json!({}),
                params_from: std::collections::BTreeMap::new(),
            },
            resolve_target: TargetResolver::Field {
                field: "payment_id".to_string(),
            },
            principal: None,
            drop_ok: false,
        }],
    );

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
    assert!(
        results[0]
            .error
            .as_ref()
            .unwrap()
            .contains("Could not resolve")
    );
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
    reg.register_tenant_rules(
        "chain",
        vec![
            ReactionRule {
                name: "confirm_triggers_authorize".to_string(),
                when: ReactionTrigger {
                    entity_type: "Order".to_string(),
                    action: Some("ConfirmOrder".to_string()),
                    to_state: Some("Confirmed".to_string()),
                    guard: None,
                },
                then: ReactionTarget {
                    entity_type: "Payment".to_string(),
                    action: "AuthorizePayment".to_string(),
                    params: serde_json::json!({}),
                    params_from: std::collections::BTreeMap::new(),
                },
                resolve_target: TargetResolver::SameId,
                principal: None,
                drop_ok: false,
            },
            ReactionRule {
                name: "authorize_triggers_capture".to_string(),
                when: ReactionTrigger {
                    entity_type: "Payment".to_string(),
                    action: Some("AuthorizePayment".to_string()),
                    to_state: Some("Authorized".to_string()),
                    guard: None,
                },
                then: ReactionTarget {
                    entity_type: "Payment".to_string(),
                    action: "CapturePayment".to_string(),
                    params: serde_json::json!({}),
                    params_from: std::collections::BTreeMap::new(),
                },
                resolve_target: TargetResolver::SameId,
                principal: None,
                drop_ok: false,
            },
        ],
    );

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
// Phase 1: params_from — cascade fires with dynamic params declared,
// missing source fields don't break the cascade (warn + skip policy).
// =========================================================================

#[test]
fn cascade_with_params_from_fires_even_when_source_fields_missing() {
    let (_guard, clock, _id_gen) = install_deterministic_context(42);

    let mut reg = ReactionRegistry::new();
    let mut params_from = std::collections::BTreeMap::new();
    // Reference a field that ConfirmOrder doesn't produce — the dispatcher
    // should log a warning and skip the key, not fail the reaction.
    params_from.insert("dynamic_key".to_string(), "missing_field".to_string());
    reg.register_tenant_rules(
        "shop-pf",
        vec![ReactionRule {
            name: "order_confirmed_with_params_from".to_string(),
            when: ReactionTrigger {
                entity_type: "Order".to_string(),
                action: Some("ConfirmOrder".to_string()),
                to_state: Some("Confirmed".to_string()),
                guard: None,
            },
            then: ReactionTarget {
                entity_type: "Payment".to_string(),
                action: "AuthorizePayment".to_string(),
                params: serde_json::json!({"static_key": "static_value"}),
                params_from,
            },
            resolve_target: TargetResolver::SameId,
            principal: None,
            drop_ok: false,
        }],
    );

    let mut sys = SimReactionSystem::new(sim_config(), reg, "shop-pf");
    sys.register_entity("order-pf1", "Order", "pf1", order_table());
    sys.register_entity("payment-pf1", "Payment", "pf1", payment_table());

    clock.advance();
    sys.step("order-pf1", "AddItem", "{}").unwrap();
    clock.advance();
    sys.step("order-pf1", "SubmitOrder", "{}").unwrap();
    clock.advance();
    sys.step("order-pf1", "ConfirmOrder", "{}").unwrap();

    sys.assert_status("order-pf1", "Confirmed");
    sys.assert_status("payment-pf1", "Authorized");

    let results = sys.last_results();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].success,
        "reaction should fire with partial params"
    );
    assert_eq!(results[0].rule_name, "order_confirmed_with_params_from");
}

#[test]
fn reactions_toml_format_parses_cleanly() {
    // Regression guard: the reactions.toml format used by paw-fs and
    // katagami-curation in the openpaw repo must parse through
    // parse_reactions. Prior to the Phase 3 audit this format was
    // effectively dead data on some paths (PascalCase resolver types
    // silently rejected by the snake_case parser).
    //
    // ADR-0046 note: the temper-repo temper-fs reactions were migrated to
    // inline [[action.triggers]] in commit 92c79fc, and the old file
    // deleted in f317b20. This test uses an inlined fixture matching the
    // same three rules paw-fs still carries in the openpaw repo.
    let source = r#"
[[reaction]]
name = "file_stream_updated_creates_version"
[reaction.when]
entity_type = "File"
action = "StreamUpdated"
to_state = "Ready"
[reaction.then]
entity_type = "FileVersion"
action = "Create"
[reaction.resolve_target]
type = "create_if_missing"
id_field = "last_version_id"

[[reaction]]
name = "file_stream_updated_supersedes_old_version"
[reaction.when]
entity_type = "File"
action = "StreamUpdated"
[reaction.then]
entity_type = "FileVersion"
action = "Supersede"
[reaction.resolve_target]
type = "field"
field = "last_version_id"

[[reaction]]
name = "file_stream_updated_increments_workspace_usage"
[reaction.when]
entity_type = "File"
action = "StreamUpdated"
[reaction.then]
entity_type = "Workspace"
action = "IncrementUsage"
[reaction.resolve_target]
type = "field"
field = "workspace_id"
"#;
    let rules = parse_reactions(source).expect("reactions.toml format must parse");
    assert_eq!(rules.len(), 3);

    let names: Vec<&str> = rules.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"file_stream_updated_creates_version"));
    assert!(names.contains(&"file_stream_updated_supersedes_old_version"));
    assert!(names.contains(&"file_stream_updated_increments_workspace_usage"));
}

#[test]
fn parse_reactions_toml_with_params_from_loads_through_registry() {
    let toml = r#"
[[reaction]]
name = "order_confirmed_pipes_payment"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
params = { source = "reaction" }
params_from = { origin_order = "order_id" }
[reaction.resolve_target]
type = "same_id"
"#;
    let rules = parse_reactions(toml).expect("parse");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].then.params_from.len(), 1);
    assert_eq!(
        rules[0]
            .then
            .params_from
            .get("origin_order")
            .map(String::as_str),
        Some("order_id")
    );
}

// =========================================================================
// Phase 3: Guard on reaction.when — rule skips when guard fails, fires
// when guard passes. Guard-skipped rules do NOT emit a ReactionResult.
// =========================================================================

#[test]
fn guard_passing_rule_fires_guard_failing_rule_skipped() {
    let (_guard, clock, _id_gen) = install_deterministic_context(42);

    // Two rules on the same trigger; one guarded state_in = Confirmed,
    // one guarded state_in = Cancelled. Only the Confirmed-guarded rule
    // should fire.
    let mut reg = ReactionRegistry::new();
    reg.register_tenant_rules(
        "shop-g",
        vec![
            ReactionRule {
                name: "fires_on_confirmed".to_string(),
                when: ReactionTrigger {
                    entity_type: "Order".to_string(),
                    action: Some("ConfirmOrder".to_string()),
                    to_state: None,
                    guard: Some(ReactionGuard::StateIn {
                        values: vec!["Confirmed".to_string()],
                    }),
                },
                then: ReactionTarget {
                    entity_type: "Payment".to_string(),
                    action: "AuthorizePayment".to_string(),
                    params: serde_json::json!({}),
                    params_from: std::collections::BTreeMap::new(),
                },
                resolve_target: TargetResolver::SameId,
                principal: None,
                drop_ok: false,
            },
            ReactionRule {
                name: "skipped_on_cancelled".to_string(),
                when: ReactionTrigger {
                    entity_type: "Order".to_string(),
                    action: Some("ConfirmOrder".to_string()),
                    to_state: None,
                    guard: Some(ReactionGuard::StateIn {
                        values: vec!["Cancelled".to_string()],
                    }),
                },
                then: ReactionTarget {
                    entity_type: "Payment".to_string(),
                    action: "FailPayment".to_string(),
                    params: serde_json::json!({}),
                    params_from: std::collections::BTreeMap::new(),
                },
                resolve_target: TargetResolver::SameId,
                principal: None,
                drop_ok: false,
            },
        ],
    );

    let mut sys = SimReactionSystem::new(sim_config(), reg, "shop-g");
    sys.register_entity("order-g1", "Order", "g1", order_table());
    sys.register_entity("payment-g1", "Payment", "g1", payment_table());

    clock.advance();
    sys.step("order-g1", "AddItem", "{}").unwrap();
    clock.advance();
    sys.step("order-g1", "SubmitOrder", "{}").unwrap();
    clock.advance();
    sys.step("order-g1", "ConfirmOrder", "{}").unwrap();

    sys.assert_status("order-g1", "Confirmed");
    sys.assert_status("payment-g1", "Authorized");

    let results = sys.last_results();
    // Only the passing rule emits a result; the skipped rule does not.
    assert_eq!(results.len(), 1, "exactly one reaction should have fired");
    assert_eq!(results[0].rule_name, "fires_on_confirmed");
}

#[test]
fn not_guard_skips_rule_when_inner_passes() {
    let (_guard, clock, _id_gen) = install_deterministic_context(42);

    // Rule guarded with Not(StateIn[Confirmed]) — should skip because
    // source post-state IS Confirmed.
    let mut reg = ReactionRegistry::new();
    reg.register_tenant_rules(
        "shop-not",
        vec![ReactionRule {
            name: "skipped_when_confirmed".to_string(),
            when: ReactionTrigger {
                entity_type: "Order".to_string(),
                action: Some("ConfirmOrder".to_string()),
                to_state: None,
                guard: Some(ReactionGuard::Not {
                    guard: Box::new(ReactionGuard::StateIn {
                        values: vec!["Confirmed".to_string()],
                    }),
                }),
            },
            then: ReactionTarget {
                entity_type: "Payment".to_string(),
                action: "AuthorizePayment".to_string(),
                params: serde_json::json!({}),
                params_from: std::collections::BTreeMap::new(),
            },
            resolve_target: TargetResolver::SameId,
            principal: None,
            drop_ok: false,
        }],
    );

    let mut sys = SimReactionSystem::new(sim_config(), reg, "shop-not");
    sys.register_entity("order-n1", "Order", "n1", order_table());
    sys.register_entity("payment-n1", "Payment", "n1", payment_table());

    clock.advance();
    sys.step("order-n1", "AddItem", "{}").unwrap();
    clock.advance();
    sys.step("order-n1", "SubmitOrder", "{}").unwrap();
    clock.advance();
    sys.step("order-n1", "ConfirmOrder", "{}").unwrap();

    sys.assert_status("order-n1", "Confirmed");
    // Payment should still be in its initial state — guard skipped the rule.
    sys.assert_status("payment-n1", "Pending");
    assert!(
        sys.last_results().is_empty(),
        "guard-skipped rule must not emit a ReactionResult"
    );
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
