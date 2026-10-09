//! Review regressions for historical admission and resident actor activity.
use super::*;
use temper_runtime::scheduler::{install_deterministic_context, sim_now};

fn remove_add_item(state: &ServerState) {
    let mut spec: toml::Value = toml::from_str(common::ORDER_IOA).unwrap();
    // Rename the input in the new spec: the old action disappears, but the
    // existing invariants and reachable lifecycle remain verifiable.
    for action in spec["action"].as_array_mut().unwrap() {
        if action["name"].as_str() == Some("AddItem") {
            action["name"] = toml::Value::String("AddProduct".into());
        }
    }
    let source = toml::to_string(&spec).unwrap();
    let result = temper_verify::cascade::VerificationCascade::from_ioa(&source)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();
    assert!(
        result.all_passed,
        "updated spec must pass verification: {:?}",
        result.levels
    );
    let mut registry = state.registry.write().unwrap();
    registry.register_tenant(
        "default",
        temper_spec::csdl::parse_csdl(common::CSDL_XML).unwrap(),
        common::CSDL_XML.into(),
        &[("Order", &source)],
    );
    registry.set_verification_status(
        &TenantId::default(),
        "Order",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: result.all_passed,
            levels: vec![],
            verified_at: "2026-09-07T00:00:00Z".into(),
        }),
    );
    assert!(
        registry
            .get_table(&TenantId::default(), "Order")
            .unwrap()
            .rules
            .iter()
            .all(|rule| rule.name != "AddItem")
    );
}

async fn create_with_history(state: &ServerState) {
    let (status, body) = post(state, "/tdata/Orders", json!({"Id":"history"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = post(
        state,
        "/tdata/Orders('history')/Temper.AddItem",
        json!({"ProductId":"p", "Quantity":1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn historical_action_removed(cold: bool) {
    for seed in [52910, 52911, 52912] {
        let (_guard, _, _) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let writer = fixture(store.clone(), "historical-writer");
        create_with_history(&writer).await;
        let state = if cold {
            fixture(store.clone(), "historical-cold")
        } else {
            writer
        };
        let before_actor = state
            .actor_registry
            .read()
            .unwrap()
            .get("default:Order:history")
            .map(|actor| actor.id().clone());
        assert_eq!(before_actor.is_none(), cold);
        remove_add_item(&state);
        let (status, body) = post(
            &state,
            "/tdata/Orders('history')/Temper.CancelOrder",
            json!({"Reason":"changed mind"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "seed={seed} cold={cold}: {body}");
        assert_eq!(body["status"], "Cancelled");
        if let Some(before_actor) = before_actor {
            assert_eq!(
                state.actor_registry.read().unwrap()["default:Order:history"].id(),
                &before_actor,
                "hot-swap must retain the resident actor"
            );
        }
        let events = store.dump_journal("default:Order:history");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event_type, "Created");
        assert_eq!(events[1].event_type, "AddItem");
        assert_eq!(events[2].event_type, "CancelOrder");
    }
}

#[tokio::test]
async fn historical_action_removed_from_spec_does_not_block_resident_action() {
    historical_action_removed(false).await;
}

#[tokio::test]
async fn historical_action_removed_from_spec_does_not_block_cold_action() {
    historical_action_removed(true).await;
}

#[tokio::test]
async fn historical_corruption_remains_a_read_error_after_spec_update() {
    let store = SimEventStore::no_faults(52913);
    let state = fixture(store.clone(), "historical-corruption");
    create_with_history(&state).await;
    remove_add_item(&state);
    let mut malformed = store.dump_journal("default:Order:history")[1].clone();
    malformed.payload = json!({"action":"AddItem"});
    store
        .append("default:Order:history", 2, &[malformed])
        .await
        .unwrap();
    let (status, body) = post(
        &state,
        "/tdata/Orders('history')/Temper.CancelOrder",
        json!({"Reason":"changed mind"}),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "ReadError");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid event for Order:history at sequence 3"),
        "{body}"
    );
    assert_eq!(store.dump_journal("default:Order:history").len(), 3);
}

#[tokio::test]
async fn resident_bound_action_activity_survives_passivation_but_idle_actor_does_not() {
    for seed in [52914, 52915, 52916] {
        let (_guard, _, _) = install_deterministic_context(seed);
        let mut state = fixture(SimEventStore::no_faults(seed), "resident-activity");
        // In-memory entities cannot be recovered after passivation. Exercise
        // that contract without a durable store masking an erroneous eviction.
        state.storage_stack = None;
        for id in ["active", "idle"] {
            let (status, body) = post(&state, "/tdata/Orders", json!({"Id":id})).await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
        let active_actor = state.actor_registry.read().unwrap()["default:Order:active"]
            .id()
            .clone();
        let aged = sim_now() - chrono::Duration::seconds(600);
        for timestamp in state.last_accessed.write().unwrap().values_mut() {
            *timestamp = aged;
        }
        let (status, body) = post(
            &state,
            "/tdata/Orders('active')/Temper.AddItem",
            json!({"ProductId":"p", "Quantity":1}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "seed={seed}: {body}");
        state.passivate_idle_actors().await;
        assert!(
            !state
                .actor_registry
                .read()
                .unwrap()
                .contains_key("default:Order:idle"),
            "genuinely idle actors must still be passivated"
        );
        assert!(
            state
                .actor_registry
                .read()
                .unwrap()
                .contains_key("default:Order:active"),
            "seed={seed}: the just-used actor was incorrectly passivated"
        );
        assert_eq!(
            state.actor_registry.read().unwrap()["default:Order:active"].id(),
            &active_actor
        );
        let (status, body) = post(
            &state,
            "/tdata/Orders('active')/Temper.AddItem",
            json!({"ProductId":"p", "Quantity":1}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "seed={seed}: {body}");
        assert_eq!(body["item_count"], 2);
    }
}
