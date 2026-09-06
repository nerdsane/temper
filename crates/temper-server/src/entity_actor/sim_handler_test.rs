use super::*;
use temper_runtime::scheduler::install_deterministic_context;

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

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

    // AddItem
    clock.advance();
    let result = handler.handle_message("AddItem", r#"{"ProductId":"laptop"}"#);
    assert!(result.is_ok());
    assert_eq!(handler.current_status(), "Draft");
    assert_eq!(handler.current_item_count(), 1);
    assert_eq!(handler.event_count(), 1);

    // SubmitOrder
    clock.advance();
    let result = handler.handle_message("SubmitOrder", "{}");
    assert!(result.is_ok());
    assert_eq!(handler.current_status(), "Submitted");
    assert_eq!(handler.event_count(), 2);
}

#[test]
fn handler_cannot_submit_empty() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let mut handler = EntityActorHandler::new("Order", "o1", order_table());
    handler.init().unwrap();

    let result = handler.handle_message("SubmitOrder", "{}");
    assert!(result.is_err());
    assert_eq!(handler.current_status(), "Draft");
}

#[test]
fn handler_valid_actions_from_draft() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let mut handler = EntityActorHandler::new("Order", "o1", order_table());
    handler.init().unwrap();

    let actions = handler.valid_actions();
    assert!(actions.contains(&"AddItem".to_string()), "got: {actions:?}");
    assert!(
        actions.contains(&"CancelOrder".to_string()),
        "got: {actions:?}"
    );
    // SubmitOrder requires items > 0, so not valid with 0 items
    assert!(
        !actions.contains(&"SubmitOrder".to_string()),
        "got: {actions:?}"
    );
}

#[test]
fn handler_valid_actions_after_add_item() {
    let (_guard, clock, _id_gen) = install_deterministic_context(42);
    let mut handler = EntityActorHandler::new("Order", "o1", order_table());
    handler.init().unwrap();

    clock.advance();
    handler.handle_message("AddItem", "{}").unwrap();

    let actions = handler.valid_actions();
    assert!(actions.contains(&"AddItem".to_string()));
    assert!(
        actions.contains(&"SubmitOrder".to_string()),
        "got: {actions:?}"
    );
    assert!(
        actions.contains(&"RemoveItem".to_string()),
        "got: {actions:?}"
    );
}

#[test]
fn handler_with_ioa_invariants_parses_spec() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let handler =
        EntityActorHandler::new("Order", "o1", order_table()).with_ioa_invariants(ORDER_IOA);

    let invariants = handler.spec_invariants();
    assert!(
        !invariants.is_empty(),
        "should have parsed invariants from IOA spec"
    );

    let names: Vec<&str> = invariants.iter().map(|i| i.name.as_str()).collect();
    assert!(
        names.contains(&"SubmitRequiresItems"),
        "should have SubmitRequiresItems, got: {names:?}"
    );
    assert!(
        names.contains(&"CancelledIsFinal"),
        "should have CancelledIsFinal, got: {names:?}"
    );
    assert!(
        !names.contains(&"ShipRequiresPayment"),
        "undeclared bool invariants should be skipped in simulation, got: {names:?}"
    );
}

#[test]
fn round_four_simulated_overflow_compares_the_logical_stored_value() {
    use super::super::actor::contract_state_tests::OVERFLOW_CONTRACT;
    let (_guard, _clock, _id_gen) = install_deterministic_context(467);
    let table = Arc::new(TransitionTable::from_ioa_source(OVERFLOW_CONTRACT));
    let mut handler = EntityActorHandler::new("Document", "sim-blob", table)
        .with_field_sync_mode(FieldSyncMode::blob_refs_default());
    handler.init().unwrap();
    let large = "S".repeat(512 * 1024);
    let written = handler
        .handle_message("Write", &serde_json::json!({"Name":large}).to_string())
        .unwrap();
    let descriptor = written["fields"]["Name"].clone();
    assert!(crate::blobs::field_overflow_descriptor(&descriptor).is_some());
    for (action, expected, accepts) in [
        ("Same", large.as_str(), true),
        ("Different", large.as_str(), false),
        ("Same", "stale", false),
        ("Different", "stale", true),
    ] {
        let before = handler.state.clone();
        let result = handler.handle_message(
            action,
            &serde_json::json!({"expected":expected}).to_string(),
        );
        assert_eq!(
            result.is_ok(),
            accepts,
            "{action}: {:?}",
            result.as_ref().err()
        );
        assert_eq!(handler.state.fields["Name"], descriptor);
        if !accepts {
            assert_eq!(
                serde_json::to_value(&handler.state).unwrap(),
                serde_json::to_value(&before).unwrap()
            );
        }
    }
}

#[test]
fn handler_without_ioa_invariants_returns_empty() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let handler = EntityActorHandler::new("Order", "o1", order_table());

    assert!(handler.spec_invariants().is_empty());
}

#[test]
fn blob_simulation_preserves_state_across_seeded_missing_and_corrupt_reads() {
    use crate::entity_actor::actor::contract_state_tests::OVERFLOW_CONTRACT;
    use temper_runtime::scheduler::DeterministicRng;
    let mut covered = std::collections::BTreeSet::new();
    for seed in 467..499 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let mut rng = DeterministicRng::new(seed);
        let table = Arc::new(TransitionTable::from_ioa_source(OVERFLOW_CONTRACT));
        let mut handler = EntityActorHandler::new("Document", format!("sim-{seed}"), table)
            .with_field_sync_mode(FieldSyncMode::blob_refs_default());
        handler.init().unwrap();
        let mut current = format!("{seed}:{}", "B".repeat(256 * 1024));
        handler
            .handle_message("Write", &serde_json::json!({"Name":current}).to_string())
            .unwrap();
        for step in 0..16 {
            let choice = rng.next_u64() % 6;
            covered.insert(choice);
            let next = format!(
                "{seed}:{step}:{}:{}",
                rng.next_u64(),
                "N".repeat(256 * 1024)
            );
            let original = handler.overflow_blobs.clone();
            if choice == 4 {
                let key = crate::blobs::field_overflow_descriptor(&handler.state.fields["Name"])
                    .unwrap()
                    .key
                    .to_owned();
                handler
                    .overflow_blobs
                    .iter_mut()
                    .find(|blob| blob.key == key)
                    .unwrap()
                    .body[0] ^= 1;
            } else if choice == 5 {
                handler.overflow_blobs.clear();
            }
            let (action, params, accepts) = match choice {
                0 => ("Write", serde_json::json!({"Name":next}), true),
                1 => ("Same", serde_json::json!({"expected":current}), true),
                2 => ("Different", serde_json::json!({"expected":next}), true),
                3 => ("Same", serde_json::json!({"expected":next}), false),
                4 => ("Same", serde_json::json!({"expected":current}), false),
                _ => ("Different", serde_json::json!({"expected":next}), false),
            };
            let before = serde_json::to_vec(&handler.state).unwrap();
            let buffer_before: Vec<_> = handler
                .overflow_blobs
                .iter()
                .map(|blob| (blob.key.clone(), blob.body.clone()))
                .collect();
            let result = handler.handle_message(action, &params.to_string());
            assert_eq!(
                result.is_ok(),
                accepts,
                "seed={seed} step={step} choice={choice}: {:?}",
                result.as_ref().err()
            );
            if !accepts {
                assert_eq!(serde_json::to_vec(&handler.state).unwrap(), before);
                assert!(handler.last_custom_effects.is_empty());
                assert!(handler.last_scheduled_actions.is_empty());
                assert_eq!(
                    handler
                        .overflow_blobs
                        .iter()
                        .map(|blob| (blob.key.clone(), blob.body.clone()))
                        .collect::<Vec<_>>(),
                    buffer_before
                );
            }
            if choice >= 4 {
                handler.overflow_blobs = original;
            } else if choice == 0 {
                current = next;
            }
            let referenced: std::collections::BTreeSet<_> = handler
                .state
                .fields
                .as_object()
                .unwrap()
                .values()
                .filter_map(crate::blobs::field_overflow_descriptor)
                .map(|descriptor| descriptor.key.to_owned())
                .collect();
            let retained: std::collections::BTreeSet<_> = handler
                .overflow_blobs
                .iter()
                .map(|blob| blob.key.clone())
                .collect();
            assert_eq!(retained, referenced, "obsolete or missing bytes");
            assert_eq!(
                handler.overflow_blobs.len(),
                retained.len(),
                "duplicate bytes"
            );
            assert!(
                retained.len() <= 2,
                "fixture has only Name and expected fields"
            );
        }
        handler.init().unwrap();
        assert!(handler.overflow_blobs.is_empty());
    }
    assert_eq!(covered, (0..6).collect());
}
