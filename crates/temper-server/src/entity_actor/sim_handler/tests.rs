use super::*;
use temper_runtime::scheduler::install_deterministic_context;

const ORDER_IOA: &str = include_str!("../../../../../test-fixtures/specs/order.ioa.toml");

fn order_table() -> Arc<TransitionTable> {
    Arc::new(TransitionTable::from_ioa_source(ORDER_IOA))
}

#[test]
fn handler_starts_in_draft() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let mut handler = EntityActorHandler::new("Order", "o1", order_table());
    handler.init().unwrap();
    assert_eq!(handler.current_status(), "Draft");
    assert_eq!(handler.current_item_count(), 0);
    assert_eq!(handler.event_count(), 0);
}

#[test]
fn handler_add_item_then_submit() {
    let (_guard, clock, _id_gen) = install_deterministic_context(42);
    let mut handler = EntityActorHandler::new("Order", "o1", order_table());
    handler.init().unwrap();
    clock.advance();
    assert!(
        handler
            .handle_message("AddItem", r#"{"ProductId":"laptop","Quantity":1}"#)
            .is_ok()
    );
    assert_eq!(handler.current_item_count(), 1);
    clock.advance();
    assert!(
        handler
            .handle_message(
                "SubmitOrder",
                r#"{"ShippingAddressId":"addr-1","PaymentMethod":"card"}"#,
            )
            .is_ok()
    );
    assert_eq!(handler.current_status(), "Submitted");
    assert_eq!(handler.event_count(), 2);
}

#[test]
fn handler_cannot_submit_empty() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let mut handler = EntityActorHandler::new("Order", "o1", order_table());
    handler.init().unwrap();
    assert!(
        handler
            .handle_message(
                "SubmitOrder",
                r#"{"ShippingAddressId":"addr-1","PaymentMethod":"card"}"#,
            )
            .is_err()
    );
    assert_eq!(handler.current_status(), "Draft");
}

#[test]
fn handler_valid_actions_follow_guards() {
    let (_guard, clock, _id_gen) = install_deterministic_context(42);
    let mut handler = EntityActorHandler::new("Order", "o1", order_table());
    handler.init().unwrap();
    let actions = handler.valid_actions();
    assert!(actions.contains(&"AddItem".to_string()));
    assert!(actions.contains(&"CancelOrder".to_string()));
    assert!(!actions.contains(&"SubmitOrder".to_string()));
    clock.advance();
    handler
        .handle_message("AddItem", r#"{"ProductId":"laptop","Quantity":1}"#)
        .unwrap();
    let actions = handler.valid_actions();
    assert!(actions.contains(&"SubmitOrder".to_string()));
    assert!(actions.contains(&"RemoveItem".to_string()));
}

#[test]
fn handler_installs_every_declared_invariant() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let handler =
        EntityActorHandler::new("Order", "o1", order_table()).with_ioa_invariants(ORDER_IOA);
    let names: Vec<&str> = handler
        .spec_invariants()
        .iter()
        .map(|invariant| invariant.name.as_str())
        .collect();
    assert!(names.contains(&"SubmitRequiresItems"));
    assert!(names.contains(&"CancelledIsFinal"));
    assert!(names.contains(&"ShipRequiresPayment"));
}

#[test]
fn handler_without_ioa_invariants_returns_empty() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let handler = EntityActorHandler::new("Order", "o1", order_table());
    assert!(handler.spec_invariants().is_empty());
}

#[test]
fn field_updates_consume_exact_sequence_in_simulation() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let mut handler = EntityActorHandler::new("Order", "o1", order_table());
    handler.init().unwrap();
    assert!(handler.update_fields(serde_json::json!({"Name": "first"}), false, Some(0)));
    assert_eq!(handler.state.sequence_nr, 1);
    assert_eq!(handler.state.fields["Name"], "first");
    assert!(!handler.update_fields(serde_json::json!({"Name": "stale"}), false, Some(0)));
    assert_eq!(handler.state.fields["Name"], "first");
}
