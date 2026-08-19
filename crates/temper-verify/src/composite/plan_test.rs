//! Tests for [`CompositeVerificationPlan`](super::CompositeVerificationPlan).

use super::*;
use temper_spec::automaton::parse_automaton;

fn order_ioa() -> &'static str {
    r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Confirmed"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
from = ["Draft"]
to = "Submitted"

[[action]]
name = "ConfirmOrder"
from = ["Submitted"]
to = "Confirmed"

[[action.triggers]]
name = "confirm_triggers_auth"
kind = "entity"
principal = "payment-service"
target_entity = "Payment"
target_action = "AuthorizePayment"

[action.triggers.resolve_target]
type = "field"
field = "payment_id"
"#
}

fn payment_ioa() -> &'static str {
    r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized"]
initial = "Pending"

[[action]]
name = "AuthorizePayment"
from = ["Pending"]
to = "Authorized"
"#
}

fn wiki_ioa() -> &'static str {
    r#"
[automaton]
name = "Wiki"
states = ["Draft", "Published"]
initial = "Draft"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
"#
}

#[test]
fn plan_from_two_entity_chain_collects_both() {
    let order = parse_automaton(order_ioa()).unwrap();
    let payment = parse_automaton(payment_ioa()).unwrap();
    let plan =
        CompositeVerificationPlan::new(&[&order, &payment], "Order").expect("plan should build");
    assert_eq!(plan.seed, "Order");
    assert_eq!(plan.scope_size(), 2, "Order + Payment in scope");
    assert_eq!(plan.edge_count(), 1);
    assert!(!plan.has_cycle);
}

#[test]
fn plan_excludes_unrelated_entities() {
    let order = parse_automaton(order_ioa()).unwrap();
    let payment = parse_automaton(payment_ioa()).unwrap();
    let wiki = parse_automaton(wiki_ioa()).unwrap();
    let plan = CompositeVerificationPlan::new(&[&order, &payment, &wiki], "Order")
        .expect("plan should build");
    assert_eq!(plan.scope_size(), 2, "Wiki excluded from Order scope");
    assert!(!plan.models.contains_key("Wiki"));
}

#[test]
fn plan_from_isolated_entity_has_no_edges() {
    let wiki = parse_automaton(wiki_ioa()).unwrap();
    let plan = CompositeVerificationPlan::new(&[&wiki], "Wiki")
        .expect("plan should build for isolated entity");
    assert_eq!(plan.scope_size(), 1);
    assert_eq!(plan.edge_count(), 0);
}

#[test]
fn missing_seed_errors() {
    let wiki = parse_automaton(wiki_ioa()).unwrap();
    let err = CompositeVerificationPlan::new(&[&wiki], "NotAnEntity")
        .expect_err("missing seed must error");
    assert!(matches!(err, CompositePlanError::SeedMissing(_)));
}

#[test]
fn trigger_pointing_to_missing_entity_errors() {
    // Order references Payment but Payment is not supplied.
    let order = parse_automaton(order_ioa()).unwrap();
    let err =
        CompositeVerificationPlan::new(&[&order], "Order").expect_err("missing target must error");
    assert!(matches!(
        err,
        CompositePlanError::UnknownTriggerTarget { .. }
    ));
}

#[test]
fn summary_renders_edges_and_scope() {
    let order = parse_automaton(order_ioa()).unwrap();
    let payment = parse_automaton(payment_ioa()).unwrap();
    let plan = CompositeVerificationPlan::new(&[&order, &payment], "Order").unwrap();
    let summary = plan.summary();
    assert!(summary.contains("Order"));
    assert!(summary.contains("Payment"));
    assert!(summary.contains("ConfirmOrder"));
    assert!(summary.contains("AuthorizePayment"));
}

#[test]
fn liveness_required_flag_propagates_to_plan() {
    let spec_with_liveness = r#"
[automaton]
name = "A"
states = ["X", "Y"]
initial = "X"

[[action]]
name = "Go"
from = ["X"]
to = "Y"

[[action.triggers]]
name = "must_fire"
kind = "entity"
liveness = "required"
target_entity = "B"
target_action = "Do"

[action.triggers.resolve_target]
type = "same_id"
"#;
    let spec_b = r#"
[automaton]
name = "B"
states = ["Idle", "Done"]
initial = "Idle"

[[action]]
name = "Do"
from = ["Idle"]
to = "Done"
"#;
    let a = parse_automaton(spec_with_liveness).unwrap();
    let b = parse_automaton(spec_b).unwrap();
    let plan = CompositeVerificationPlan::new(&[&a, &b], "A").unwrap();
    assert!(plan.requires_liveness());
}

#[test]
fn sidecar_related_pair_joins_isolated_entities() {
    let curator = parse_automaton(
        r#"
[automaton]
name = "HumanCurator"
states = ["Reviewing", "Published"]
initial = "Reviewing"

[[action]]
name = "Publish"
from = ["Reviewing"]
to = "Published"
"#,
    )
    .unwrap();
    let review = parse_automaton(
        r#"
[automaton]
name = "ReviewAgent"
states = ["Reviewing", "VerdictRecorded"]
initial = "Reviewing"

[[action]]
name = "RecordVerdict"
from = ["Reviewing"]
to = "VerdictRecorded"
"#,
    )
    .unwrap();
    let isolated = CompositeVerificationPlan::new(&[&curator, &review], "HumanCurator").unwrap();
    assert_eq!(isolated.scope_size(), 1);
    let sidecar = temper_spec::parse_cross_invariants(
        r#"
[[invariant]]
name = "PublishNeedsThisReviewRecorded"
on = "HumanCurator.Publish"
assert = 'related(ReviewAgent, review_agent_id).status in ["VerdictRecorded"]'
"#,
    )
    .unwrap();
    let joined = CompositeVerificationPlan::new_with_sidecar(
        &[&curator, &review],
        "HumanCurator",
        Some(&sidecar),
    )
    .unwrap();
    assert_eq!(joined.scope_size(), 2);
    assert!(joined.models.contains_key("ReviewAgent"));
    assert_eq!(joined.related_field_rules.len(), 1);
}
