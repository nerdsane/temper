//! `[[invariant]]`s hold on every move.
//!
//! A move that would make an invariant false, when it held before the move, is
//! refused with the invariant's name, and the record stays as it was. A record
//! that already breaks an invariant (written before the rule existed) can
//! still move.
//!
//! The moves are dispatched the way every caller dispatches one, a state
//! timer's fire, a reaction and a module's answer included: all of them reach
//! the one function that evaluates a move.

mod common;

use std::sync::Arc;

use serde_json::json;
use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{FaultConfig, SimActorSystem, SimActorSystemConfig};
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::entity_actor::{EntityActorHandler, EntityResponse};
use temper_server::request_context::AgentContext;

/// An order whose `SubmitOrder` has no guard: only the invariant keeps an
/// empty order out of `Submitted`.
const ORDER: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Shipped"]
initial = "Draft"

[[state]]
name = "items"
type = "counter"
initial = 0

[[action]]
name = "AddItem"
kind = "input"
from = ["Draft"]
effect = ["items += 1"]

[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"
params = ["Notes"]

[[action]]
name = "ShipOrder"
kind = "input"
from = ["Submitted"]
to = "Shipped"

[[invariant]]
name = "SubmittedOrdersHaveItems"
assert = "status in ['Submitted', 'Shipped'] => items > 0"
"#;

const INVARIANT: &str = "SubmittedOrdersHaveItems";

/// The same order before the rule existed.
fn order_without_invariant() -> &'static str {
    ORDER
        .split("[[invariant]]")
        .next()
        .expect("the spec has text before its invariant")
}

fn state_with(spec: &str, name: &str) -> ServerState {
    let (state, _) = common::build_single_tenant_state(0, name, "default", &[("Order", spec)]);
    state
}

async fn dispatch(
    state: &ServerState,
    id: &str,
    action: &str,
    params: serde_json::Value,
) -> EntityResponse {
    match state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            id,
            action,
            params,
            &AgentContext::default(),
        )
        .await
    {
        Ok(response) => response,
        Err(error) => panic!("{action} on {id} did not answer: {error}"),
    }
}

async fn read(state: &ServerState, id: &str) -> EntityResponse {
    state
        .get_tenant_entity_state(&TenantId::default(), "Order", id)
        .await
        .expect("the order reads")
}

#[tokio::test]
async fn a_move_that_would_break_an_invariant_is_refused_with_its_name() {
    let state = state_with(ORDER, "invariant-refused");
    state
        .get_or_create_tenant_entity(&TenantId::default(), "Order", "empty", json!({}))
        .await
        .expect("the order is created");
    let before = read(&state, "empty").await.state;

    let response = dispatch(&state, "empty", "SubmitOrder", json!({"Notes": "rush"})).await;
    assert!(
        !response.success,
        "SubmitOrder on an empty order makes {INVARIANT} false, and it went through to '{}'",
        response.state.status
    );
    let refusal = serde_json::to_value(&response.refusal).expect("a refusal serializes");
    assert_eq!(refusal["code"], "InvariantViolated", "{refusal}");
    assert_eq!(refusal["invariant"], INVARIANT, "{refusal}");
    let error = response.error.unwrap_or_default();
    assert!(
        error.contains(INVARIANT),
        "the message names the invariant: {error}"
    );

    // Nothing of the refused move stays: not its status, its counters, the
    // param it would have written, nor its event.
    let after = read(&state, "empty").await.state;
    assert_eq!(after.status, "Draft");
    assert_eq!(after.counters, before.counters);
    assert_eq!(after.fields, before.fields);
    assert_eq!(after.sequence_nr, before.sequence_nr);

    // With an item the invariant still holds after the move, which goes through.
    assert!(
        dispatch(&state, "empty", "AddItem", json!({}))
            .await
            .success
    );
    let response = dispatch(&state, "empty", "SubmitOrder", json!({"Notes": "rush"})).await;
    assert!(response.success, "{:?}", response.error);
    assert_eq!(response.state.status, "Submitted");
}

#[tokio::test]
async fn a_record_already_breaking_an_invariant_can_still_move() {
    let state = state_with(order_without_invariant(), "invariant-already-broken");
    state
        .get_or_create_tenant_entity(&TenantId::default(), "Order", "old", json!({}))
        .await
        .expect("the order is created");
    // Before the rule existed an empty order could be submitted.
    let response = dispatch(&state, "old", "SubmitOrder", json!({})).await;
    assert!(response.success, "{:?}", response.error);

    // The rule arrives, and the order already breaks it.
    {
        let mut registry = state.registry.write().unwrap();
        let csdl = temper_spec::csdl::parse_csdl(common::CSDL_XML).expect("CSDL parses");
        registry.register_tenant(
            "default",
            csdl,
            common::CSDL_XML.to_string(),
            &[("Order", ORDER)],
        );
    }

    // It is not held where it stands: it still moves, and still breaks it.
    let response = dispatch(&state, "old", "ShipOrder", json!({})).await;
    assert!(
        response.success,
        "an order already breaking {INVARIANT} could not move: {:?}",
        response.error
    );
    assert_eq!(response.state.status, "Shipped");
}

/// Random moves over many seeds. The simulator checks each invariant after
/// every move that went through, so a move that broke one is a violation.
#[test]
fn random_moves_never_break_an_invariant() {
    let table = Arc::new(TransitionTable::from_ioa_source(ORDER));
    let mut shipped = 0;
    for seed in 0..64 {
        let mut sim = SimActorSystem::new(SimActorSystemConfig {
            seed,
            max_ticks: 200,
            faults: FaultConfig::none(),
            max_actions_per_actor: 20,
        });
        for id in ["o1", "o2", "o3"] {
            sim.register_actor(
                id,
                Box::new(
                    EntityActorHandler::new("Order", id, table.clone()).with_ioa_invariants(ORDER),
                ),
            );
        }
        let result = sim.run_random();
        assert!(
            result.all_invariants_held,
            "seed {seed}: a move broke an invariant that held: {:?}",
            result.violations
        );
        shipped += result
            .actor_states
            .iter()
            .filter(|(_, status, _, _)| status == "Shipped")
            .count();
    }
    // Refusing the empty orders' submits must not stop the rest: orders
    // with items still get through to the end.
    assert!(shipped > 0, "no order was shipped in any seed");
}
