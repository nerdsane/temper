//! Tests for the composite joint-state model (ADR-0046 / ADR-0150).

use super::*;
use crate::composite::CompositeVerificationPlan;
use stateright::Checker;
use temper_spec::automaton::parse_automaton;

fn order_ioa() -> &'static str {
    r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Confirmed"]

[[action]]
name = "ConfirmOrder"
from = ["Draft"]
to = "Confirmed"

[[action.triggers]]
name = "auth_payment"
kind = "entity"
target_entity = "Payment"
target_action = "AuthorizePayment"
# Payment may be authorized independently before the Order's reaction fires;
# in this toy model that is benign convergence, so the reaction is best-effort.
drop_ok = true

[action.triggers.resolve_target]
type = "same_id"
"#
}

fn payment_ioa() -> &'static str {
    r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized"]
initial = "Pending"
allow_indefinite_states = ["Pending", "Authorized"]

[[action]]
name = "AuthorizePayment"
from = ["Pending"]
to = "Authorized"
"#
}

#[test]
fn bfs_explores_joint_state_space_with_cascade() {
    let order = parse_automaton(order_ioa()).unwrap();
    let payment = parse_automaton(payment_ioa()).unwrap();
    let plan = CompositeVerificationPlan::new(&[&order, &payment], "Order").unwrap();
    let model = CompositeTemperModel::from_plan(plan);

    let init_states = model.init_states();
    assert_eq!(init_states.len(), 1);
    let init = &init_states[0];
    assert_eq!(init.entities.get("Order").unwrap().status, "Draft");
    assert_eq!(init.entities.get("Payment").unwrap().status, "Pending");

    // Fire ConfirmOrder — cascade should advance Payment to Authorized.
    let mut actions = Vec::new();
    model.actions(init, &mut actions);
    let confirm = actions
        .iter()
        .find(|a| a.entity == "Order" && a.action.name == "ConfirmOrder")
        .expect("Order.ConfirmOrder enabled from Draft");
    let after = model.next_state(init, confirm.clone()).unwrap();
    assert_eq!(after.entities.get("Order").unwrap().status, "Confirmed");
    assert_eq!(
        after.entities.get("Payment").unwrap().status,
        "Authorized",
        "cascade should have triggered Payment.AuthorizePayment"
    );
}

#[test]
fn bfs_checker_proves_joint_invariant_holds() {
    // Run Stateright BFS over the composite model — ensures no
    // joint state violates the local invariants.
    let order = parse_automaton(order_ioa()).unwrap();
    let payment = parse_automaton(payment_ioa()).unwrap();
    let plan = CompositeVerificationPlan::new(&[&order, &payment], "Order").unwrap();
    let model = CompositeTemperModel::from_plan(plan);
    let checker = model.checker().spawn_bfs().join();
    // "always" properties emit no discoveries when they hold.
    let discoveries = checker.discoveries();
    assert!(
        discoveries.is_empty(),
        "unexpected discoveries: {discoveries:?}"
    );
    // Ensure BFS actually visited multiple states.
    assert!(checker.unique_state_count() >= 2);
}

#[test]
fn self_loop_cascade_bounded_by_max_depth() {
    // Self-triggering entity (Assign → Start on same entity) should
    // not blow up the state space; cascade bound stops it.
    let spec = r#"
[automaton]
name = "Agent"
states = ["Idle", "Assigned", "Working"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Assigned", "Working"]

[[action]]
name = "Assign"
from = ["Idle"]
to = "Assigned"

[[action.triggers]]
name = "auto_start"
kind = "entity"
to_state = "Assigned"
target_entity = "Agent"
target_action = "Start"

[action.triggers.resolve_target]
type = "same_id"

[[action]]
name = "Start"
from = ["Assigned"]
to = "Working"
"#;
    let agent = parse_automaton(spec).unwrap();
    let plan = CompositeVerificationPlan::new(&[&agent], "Agent").unwrap();
    let model = CompositeTemperModel::from_plan(plan);
    let init = &model.init_states()[0];
    let mut actions = Vec::new();
    model.actions(init, &mut actions);
    let assign = actions
        .iter()
        .find(|a| a.action.name == "Assign")
        .cloned()
        .unwrap();
    let after = model.next_state(init, assign).unwrap();
    // The inline cascade fires Start automatically once — Agent lands in Working.
    assert_eq!(after.entities.get("Agent").unwrap().status, "Working");
}
