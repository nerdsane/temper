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

#[test]
fn composite_initialization_covers_cross_entity_identity_partitions() {
    let left = parse_automaton(
        r#"
[automaton]
name = "Left"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "node_id"
type = "ref"
entity_type = "Node"
initial = ""

[[key]]
name = "left_node"
properties = ["node_id"]
entity_id = true

[[action]]
name = "Ping"
from = ["Active"]
to = "Active"

[[action.triggers]]
name = "ping_right"
kind = "entity"
target_entity = "Right"
target_action = "Observe"
drop_ok = true

[action.triggers.resolve_target]
type = "same_id"
"#,
    )
    .unwrap();
    let right = parse_automaton(
        r#"
[automaton]
name = "Right"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "node_id"
type = "ref"
entity_type = "Node"
initial = ""

[[key]]
name = "right_node"
properties = ["node_id"]
entity_id = true

[[action]]
name = "Observe"
from = ["Active"]
to = "Active"
"#,
    )
    .unwrap();
    let plan = CompositeVerificationPlan::new(&[&left, &right], "Left").unwrap();
    let states = CompositeTemperModel::from_plan(plan).init_states();
    assert_eq!(states.len(), 2);
    assert!(states.iter().any(|state| {
        state.entities["Left"].counters["__ref:node_id"]
            == state.entities["Right"].counters["__ref:node_id"]
    }));
    assert!(states.iter().any(|state| {
        state.entities["Left"].counters["__ref:node_id"]
            != state.entities["Right"].counters["__ref:node_id"]
    }));
}

#[test]
fn composite_transition_applies_typed_reference_projection() {
    let document = parse_automaton(
        r#"
[automaton]
name = "Document"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "workspace_id"
type = "ref"
entity_type = "Workspace"
initial = ""

[[action]]
name = "Attach"
from = ["Active"]
to = "Active"
params = [{ name = "workspace_id", type = "ref", entity_type = "Workspace" }]
"#,
    )
    .unwrap();
    let plan = CompositeVerificationPlan::new(&[&document], "Document").unwrap();
    let model = CompositeTemperModel::from_plan(plan);
    let initial = model.init_states().remove(0);
    let mut actions = Vec::new();
    model.actions(&initial, &mut actions);
    let attach = actions
        .into_iter()
        .find(|action| {
            action.action.name == "Attach" && action.action.reference_params["workspace_id"] == 1
        })
        .unwrap();
    let attached = model.next_state(&initial, attach).unwrap();
    assert_eq!(
        attached.entities["Document"].counters["__ref:workspace_id"],
        1
    );
}

#[test]
fn composite_actions_use_the_joint_reference_namespace() {
    let left = parse_automaton(
        r#"
[automaton]
name = "Left"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "first_id"
type = "ref"
entity_type = "Target"
initial = ""

[[state]]
name = "second_id"
type = "ref"
entity_type = "Target"
initial = ""

[[key]]
name = "left_targets"
properties = ["first_id", "second_id"]
entity_id = true

[[action]]
name = "Ping"
from = ["Active"]
to = "Active"

[[action.triggers]]
name = "observe_right"
kind = "entity"
target_entity = "Right"
target_action = "Observe"
drop_ok = true

[action.triggers.resolve_target]
type = "same_id"
"#,
    )
    .unwrap();
    let right = parse_automaton(
        r#"
[automaton]
name = "Right"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "owner_id"
type = "ref"
entity_type = "Target"
initial = ""

[[key]]
name = "right_target"
properties = ["owner_id"]
entity_id = true

[[action]]
name = "Observe"
from = ["Active"]
to = "Active"

[[action]]
name = "Confirm"
from = ["Active"]
to = "Active"
params = [{ name = "candidate", type = "ref", entity_type = "Target" }]
guard = [{ type = "reference_equals", reference = "owner_id", param = "candidate" }]

[[action]]
name = "Explore"
from = ["Active"]
to = "Active"
params = [{ name = "candidate", type = "ref", entity_type = "Target" }]
"#,
    )
    .unwrap();
    let plan = CompositeVerificationPlan::new(&[&left, &right], "Left").unwrap();
    let model = CompositeTemperModel::from_plan(plan);
    let state = model
        .init_states()
        .into_iter()
        .find(|state| {
            state.entities["Left"].counters["__ref:first_id"] == 1
                && state.entities["Left"].counters["__ref:second_id"] == 2
                && state.entities["Right"].counters["__ref:owner_id"] == 3
        })
        .expect("joint partition with three distinct stored identities");
    let mut actions = Vec::new();
    model.actions(&state, &mut actions);
    let confirms = actions
        .iter()
        .filter(|action| action.entity == "Right" && action.action.name == "Confirm")
        .collect::<Vec<_>>();
    assert_eq!(confirms.len(), 1);
    assert_eq!(confirms[0].action.reference_params["candidate"], 3);
    let explored = actions
        .iter()
        .filter(|action| action.entity == "Right" && action.action.name == "Explore")
        .map(|action| action.action.reference_params["candidate"])
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(explored, std::collections::BTreeSet::from([1, 2, 3, 4]));
}
