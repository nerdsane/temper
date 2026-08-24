use super::*;

#[tokio::test]
async fn scoped_timeout_recovers_against_its_retired_exact_bundle() {
    let (_guard, clock, _ids) = install_deterministic_context(915);
    let tenant = TenantId::new("scoped-timeout-tenant");
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-915".into(),
    };
    let digest = format!("sha256:{}", "8".repeat(64));
    let pin = SchemaExecutionPin {
        scope: scope.clone(),
        bundle_digest: digest.clone(),
    };
    let store = SimEventStore::no_faults(915);
    activate_durable_pin(&tenant, &pin, &store, SCOPED_TIMEOUT_ORDER_IOA, None).await;
    let state = scoped_state(
        &tenant,
        &scope,
        &digest,
        store.clone(),
        SCOPED_TIMEOUT_ORDER_IOA,
    );
    state
        .get_or_create_scoped_entity(
            &tenant,
            "Order",
            "order-timeout",
            serde_json::json!({}),
            pin.clone(),
        )
        .await
        .expect("create scoped timed entity");

    let source_id = format!(
        "{tenant}:Order:{}",
        temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
            "order-timeout",
            &pin,
        )
    );
    let intent = store
        .dump_journal(&source_id)
        .iter()
        .find(|event| event.event_type == "Created")
        .and_then(|event| extract_intents(&event.payload).ok())
        .and_then(|intents| intents.into_iter().next())
        .expect("scoped Created event should carry a timeout intent");
    assert_eq!(
        intent.schema_pin.as_ref().map(|value| &value.execution),
        Some(&pin)
    );
    assert_eq!(
        intent
            .state_timeout
            .as_ref()
            .map(|timeout| timeout.schema_digest.as_str()),
        Some(digest.as_str())
    );
    drop(state);

    let active = store
        .active_schema_pointer(tenant.as_str(), &scope)
        .await
        .expect("active pointer lookup")
        .expect("active pointer");
    store
        .retire_schema_bundle(RetireSchemaBundle {
            tenant: tenant.to_string(),
            scope: scope.clone(),
            bundle_digest: digest.clone(),
            expected_fence: active.fence,
            operation: SchemaOperationIdentity {
                idempotency_key: "retire-scoped-timeout".into(),
                request_digest: format!("sha256:{}", "6".repeat(64)),
                request_id: "retire-scoped-timeout".into(),
            },
        })
        .await
        .expect("retire scoped timeout bundle");

    let mut restarted = ServerState::from_registry(
        ActorSystem::new("scoped-timeout-restart"),
        SpecRegistry::new(),
    );
    restarted
        .authz
        .reload_tenant_policies(tenant.as_str(), "permit(principal, action, resource);")
        .expect("timeout fixture policy");
    restarted.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    clock.advance_by(11);
    let dispatcher = restarted
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("timeout dispatcher");
    let (first, second) = tokio::join!(
        dispatcher.dispatch_committed_intent(&restarted, intent.clone()),
        dispatcher.dispatch_committed_intent(&restarted, intent),
    );
    first.expect("first pending timeout owner should converge");
    second.expect("duplicate pending timeout owner should converge");

    assert_eq!(
        restarted
            .get_scoped_entity_state(&tenant, "Order", "order-timeout", pin.clone())
            .await
            .expect("hydrate scoped entity at retired pin")
            .state
            .status,
        "Expired"
    );
    assert_eq!(
        store
            .dump_journal(&source_id)
            .iter()
            .filter(|event| event.event_type == "Expire")
            .count(),
        1
    );
}

#[tokio::test]
async fn scoped_timeout_is_suppressed_after_durable_migration_cutover() {
    let (_guard, clock, _ids) = install_deterministic_context(916);
    let tenant = TenantId::new("scoped-timeout-migration-tenant");
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-916".into(),
    };
    let source_digest = format!("sha256:{}", "7".repeat(64));
    let target_digest = format!("sha256:{}", "6".repeat(64));
    let source_pin = SchemaExecutionPin {
        scope: scope.clone(),
        bundle_digest: source_digest.clone(),
    };
    let target_pin = SchemaExecutionPin {
        scope: scope.clone(),
        bundle_digest: target_digest.clone(),
    };
    let store = SimEventStore::no_faults(916);
    activate_durable_pin(&tenant, &source_pin, &store, SCOPED_TIMEOUT_ORDER_IOA, None).await;
    let state = scoped_state(
        &tenant,
        &scope,
        &source_digest,
        store.clone(),
        SCOPED_TIMEOUT_ORDER_IOA,
    );
    state
        .get_or_create_scoped_entity(
            &tenant,
            "Order",
            "order-migrated",
            serde_json::json!({}),
            source_pin.clone(),
        )
        .await
        .expect("create source-pinned timed entity");
    let source_id = format!(
        "{tenant}:Order:{}",
        temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
            "order-migrated",
            &source_pin,
        )
    );
    let intent = store
        .dump_journal(&source_id)
        .iter()
        .find(|event| event.event_type == "Created")
        .and_then(|event| extract_intents(&event.payload).ok())
        .and_then(|intents| intents.into_iter().next())
        .expect("source clock should be durable");
    drop(state);

    activate_durable_pin(
        &tenant,
        &target_pin,
        &store,
        SCOPED_TIMEOUT_ORDER_IOA,
        Some(source_digest.clone()),
    )
    .await;
    let target_id = format!(
        "{tenant}:Order:{}",
        temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
            "order-migrated",
            &target_pin,
        )
    );
    let migration_event = temper_server::entity_actor::EntityEvent {
        action: "$temper.fields.updated.v1".into(),
        from_status: "Draft".into(),
        to_status: "Draft".into(),
        timestamp: sim_now(),
        params: serde_json::json!({"migration": true}),
        idempotency_key: None,
    };
    store
        .append(
            &target_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "$temper.fields.updated.v1".into(),
                payload: serde_json::to_value(migration_event).expect("migration event payload"),
                metadata: EventMetadata {
                    event_id: temper_runtime::scheduler::sim_uuid(),
                    causation_id: temper_runtime::scheduler::sim_uuid(),
                    correlation_id: temper_runtime::scheduler::sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: target_id.clone(),
                },
            }],
        )
        .await
        .expect("migration target marker should commit");

    let mut restarted = ServerState::from_registry(
        ActorSystem::new("scoped-timeout-migration-restart"),
        SpecRegistry::new(),
    );
    restarted
        .authz
        .reload_tenant_policies(tenant.as_str(), "permit(principal, action, resource);")
        .expect("timeout fixture policy");
    restarted.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    clock.advance_by(11);
    let dispatcher = restarted
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("timeout dispatcher");
    dispatcher
        .dispatch_committed_intent(&restarted, intent)
        .await
        .expect("migrated clock should terminate cleanly");

    assert_eq!(
        store
            .dump_journal(&source_id)
            .iter()
            .filter(|event| event.event_type == "Expire")
            .count(),
        0,
        "retired source clock must not mutate after migration cutover"
    );
    let records = temper_server::trigger::delivery::list_delivery_records(
        &BoxedEventStore::new(store),
        tenant.as_str(),
        10,
    )
    .await
    .expect("delivery records");
    assert!(records.iter().any(|(record, _)| {
        record.status == ReactionDeliveryStatus::Skipped
            && record
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("migrated"))
    }));
}

#[tokio::test]
async fn scoped_timeout_append_is_fenced_when_migration_cuts_over_after_validation() {
    let (_guard, clock, _ids) = install_deterministic_context(917);
    let tenant = TenantId::new("scoped-timeout-cutover-race-tenant");
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-917".into(),
    };
    let source_pin = SchemaExecutionPin {
        scope: scope.clone(),
        bundle_digest: format!("sha256:{}", "4".repeat(64)),
    };
    let target_pin = SchemaExecutionPin {
        scope,
        bundle_digest: format!("sha256:{}", "3".repeat(64)),
    };
    let store = SimEventStore::no_faults(917);
    activate_durable_pin(&tenant, &source_pin, &store, SCOPED_TIMEOUT_ORDER_IOA, None).await;
    let state = scoped_state(
        &tenant,
        &source_pin.scope,
        &source_pin.bundle_digest,
        store.clone(),
        SCOPED_TIMEOUT_ORDER_IOA,
    );
    state
        .get_or_create_scoped_entity(
            &tenant,
            "Order",
            "order-cutover-race",
            serde_json::json!({}),
            source_pin.clone(),
        )
        .await
        .expect("create source-pinned timed entity");
    let source_id = format!(
        "{tenant}:Order:{}",
        temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
            "order-cutover-race",
            &source_pin,
        )
    );
    let source_events = store.dump_journal(&source_id);
    let intent = source_events
        .iter()
        .find(|event| event.event_type == "Created")
        .and_then(|event| extract_intents(&event.payload).ok())
        .and_then(|intents| intents.into_iter().next())
        .expect("source clock should be durable");
    let migration_fence = prepare_empty_migration(
        &tenant,
        &source_pin,
        &target_pin,
        &store,
        u64::try_from(source_events.len()).expect("source event count fits u64"),
    )
    .await;
    drop(state);

    let restarted = scoped_state(
        &tenant,
        &source_pin.scope,
        &source_pin.bundle_digest,
        store.clone(),
        SCOPED_TIMEOUT_ORDER_IOA,
    );
    restarted
        .authz
        .reload_tenant_policies(tenant.as_str(), "permit(principal, action, resource);")
        .expect("timeout fixture policy");
    clock.advance_by(11);
    store.inject_append_delay(&source_id, std::time::Duration::from_millis(100));
    let dispatcher = restarted
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("timeout dispatcher");
    let dispatch = dispatcher.dispatch_committed_intent(&restarted, intent);
    let cutover = async {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        store
            .cut_over_schema_migration(
                tenant.as_str(),
                "state-timeout-cutover-race",
                migration_fence,
                "state-timeout-validation",
            )
            .await
    };
    let (dispatch_result, cutover_result) = tokio::join!(dispatch, cutover);
    dispatch_result.expect("fenced timeout should terminate cleanly");
    cutover_result.expect("migration cutover should commit during delayed append");

    assert_eq!(
        store
            .dump_journal(&source_id)
            .iter()
            .filter(|event| event.event_type == "Expire")
            .count(),
        0,
        "append-time fence must prevent a retired-journal timeout"
    );
    let records = temper_server::trigger::delivery::list_delivery_records(
        &BoxedEventStore::new(store),
        tenant.as_str(),
        10,
    )
    .await
    .expect("delivery records");
    assert!(
        records.iter().any(|(record, _)| {
            record.status == ReactionDeliveryStatus::Skipped
                && record
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.contains("migrated scoped schema write fence"))
        }),
        "records: {records:#?}"
    );
}
