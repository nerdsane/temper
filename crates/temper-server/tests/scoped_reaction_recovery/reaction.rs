use super::*;

#[tokio::test]
async fn scoped_durable_reaction_materializes_and_reconciles_at_exact_pin() {
    let (_guard, _clock, _ids) = install_deterministic_context(914);
    let tenant = TenantId::new("scoped-reaction-tenant");
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-914".into(),
    };
    let digest = format!("sha256:{}", "9".repeat(64));
    let pin = SchemaExecutionPin {
        scope: scope.clone(),
        bundle_digest: digest.clone(),
    };
    let store = SimEventStore::no_faults(914);
    activate_durable_pin(&tenant, &pin, &store, SCOPED_ORDER_IOA, None).await;
    let state = scoped_state(&tenant, &scope, &digest, store.clone(), SCOPED_ORDER_IOA);
    let context = AgentContext {
        schema_pin: Some(pin.clone()),
        ..AgentContext::default()
    };
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "order-1",
            "ConfirmOrder",
            serde_json::json!({}),
            &context,
        )
        .await
        .expect("scoped source action should dispatch");

    let source_id = format!(
        "{tenant}:Order:{}",
        temper_runtime::persistence::schema_deployment::scoped_journal_entity_id("order-1", &pin,)
    );
    let source = store.dump_journal(&source_id);
    let intent = extract_intents(
        &source
            .iter()
            .find(|event| event.event_type == "ConfirmOrder")
            .expect("source event should exist")
            .payload,
    )
    .expect("source intent should decode")
    .pop()
    .expect("source intent should exist");
    assert_eq!(
        intent.schema_pin.as_ref().map(|value| &value.execution),
        Some(&pin)
    );

    let lifecycle_id = delivery_journal_id(&intent);
    let lifecycle = store.dump_journal(&lifecycle_id);
    let mut ambiguous: ReactionDeliveryRecord = serde_json::from_value(
        lifecycle
            .last()
            .expect("delivery lifecycle should exist")
            .payload
            .clone(),
    )
    .expect("delivery lifecycle should decode");
    ambiguous.status = ReactionDeliveryStatus::Dispatching;
    ambiguous.lease_expires_at = Some(sim_now() - chrono::Duration::seconds(1));
    append_delivery_record(
        &BoxedEventStore::new(store.clone()),
        lifecycle
            .last()
            .expect("delivery sequence should exist")
            .sequence_nr,
        &ambiguous,
    )
    .await
    .expect("ambiguous response-loss state should persist");
    drop(state);

    let active = store
        .active_schema_pointer(tenant.as_str(), &scope)
        .await
        .expect("active pointer lookup should succeed")
        .expect("active pointer should exist");
    store
        .retire_schema_bundle(RetireSchemaBundle {
            tenant: tenant.to_string(),
            scope: scope.clone(),
            bundle_digest: digest.clone(),
            expected_fence: active.fence,
            operation: SchemaOperationIdentity {
                idempotency_key: "retire-scoped-reaction".into(),
                request_digest: format!("sha256:{}", "5".repeat(64)),
                request_id: "retire-scoped-reaction".into(),
            },
        })
        .await
        .expect("durable bundle should retire before recovery");

    let mut restarted = ServerState::from_registry(
        ActorSystem::new("scoped-reaction-restart"),
        SpecRegistry::new(),
    );
    restarted.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    restarted.rebuild_reaction_dispatcher();
    assert!(
        restarted
            .registry
            .read()
            .expect("registry lock")
            .get_scoped_config_at_digest(&tenant, &scope, &digest)
            .is_none(),
        "restart fixture must begin without manually hydrated scoped metadata"
    );
    let dispatcher = restarted
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("dispatcher should exist");
    dispatcher
        .dispatch_committed_intent(&restarted, intent)
        .await
        .expect("scoped receipt reconciliation should succeed");

    let target_id = format!(
        "{tenant}:Payment:{}",
        temper_runtime::persistence::schema_deployment::scoped_journal_entity_id("order-1", &pin,)
    );
    assert_eq!(
        store
            .dump_journal(&target_id)
            .iter()
            .filter(|event| event.event_type == "AuthorizePayment")
            .count(),
        1,
        "recovery must not duplicate the pinned target event"
    );
}
