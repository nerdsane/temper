use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creation_into_timed_initial_state_fires_without_later_traffic() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-state-timeout-create-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let tenant = TenantId::new("tenant-a");
    let store = open_store(&db_url).await;
    let inspection_store = BoxedEventStore::new(store.clone());
    let state = build_state("timeout-create", store);

    let created = state
        .get_or_create_tenant_entity(&tenant, "Ticket", "ticket-create", serde_json::json!({}))
        .await
        .expect("create timed entity");
    assert_eq!(created.state.status, "Open");

    let status = wait_for_status(
        &state,
        &tenant,
        "ticket-create",
        "InProgress",
        Duration::from_secs(10),
    )
    .await;
    if status != "InProgress" {
        let records = temper_server::trigger::delivery::list_delivery_records(
            &inspection_store,
            tenant.as_str(),
            10,
        )
        .await
        .expect("delivery records");
        panic!("timeout stayed in {status}; durable records: {records:#?}");
    }

    let source = inspection_store
        .read_events("tenant-a:Ticket:ticket-create", 0)
        .await
        .expect("read timeout source journal");
    let intent = source
        .iter()
        .find(|event| event.event_type == "Created")
        .and_then(|event| temper_server::trigger::delivery::extract_intents(&event.payload).ok())
        .and_then(|intents| intents.into_iter().next())
        .expect("Created event should carry the timeout intent");
    let dispatcher = state
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("durable timeout dispatcher");
    let (first, second) = tokio::join!(
        dispatcher.dispatch_committed_intent(&state, intent.clone()),
        dispatcher.dispatch_committed_intent(&state, intent),
    );
    first.expect("duplicate wakeup should reconcile");
    second.expect("duplicate wakeup should reconcile");
    let source = inspection_store
        .read_events("tenant-a:Ticket:ticket-create", 0)
        .await
        .expect("reread timeout source journal");
    assert_eq!(
        source
            .iter()
            .filter(|event| event.event_type == "AssignAgent")
            .count(),
        1,
        "duplicate scheduler wakeups must converge on one target event"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_timeout_authority_is_narrow_and_fails_closed_when_denied() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-state-timeout-denied-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let tenant = TenantId::new("tenant-a");
    let store = open_store(&db_url).await;
    let inspection_store = BoxedEventStore::new(store.clone());
    let state = build_state_with_policy(
        "timeout-denied",
        store,
        r#"permit(principal is Customer, action, resource) when {
            principal.id == "anonymous"
        };"#,
    );
    state
        .get_or_create_tenant_entity(&tenant, "Ticket", "ticket-denied", serde_json::json!({}))
        .await
        .expect("create timed entity");
    tokio::time::sleep(Duration::from_millis(1_250)).await; // determinism-ok: pass bootstrap clock deadline

    assert_eq!(
        state
            .get_tenant_entity_state(&tenant, "Ticket", "ticket-denied")
            .await
            .expect("load denied entity")
            .state
            .status,
        "Open"
    );
    let records = temper_server::trigger::delivery::list_delivery_records(
        &inspection_store,
        tenant.as_str(),
        10,
    )
    .await
    .expect("delivery records");
    assert!(
        records.iter().any(|(record, _)| {
            record.status == temper_server::trigger::delivery::ReactionDeliveryStatus::Rejected
        }),
        "denied bootstrap lifecycle: {records:#?}"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callers_cannot_forge_timeout_occurrence_evidence() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-state-timeout-forgery-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let tenant = TenantId::new("tenant-a");
    let store = open_store(&db_url).await;
    let inspection_store = BoxedEventStore::new(store.clone());
    let state = build_state("timeout-forgery", store);
    state
        .get_or_create_tenant_entity(&tenant, "Ticket", "ticket-forgery", serde_json::json!({}))
        .await
        .expect("create timed entity");
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Ticket",
            "ticket-forgery",
            "Close",
            serde_json::json!({
                "_temper_state_timeout_declaration_v1": "Open"
            }),
            &temper_server::request_context::AgentContext::default(),
        )
        .await
        .expect("ordinary action should dispatch");
    assert_eq!(response.state.state_timeout_occurrences("Open"), 0);
    let events = inspection_store
        .read_events("tenant-a:Ticket:ticket-forgery", 0)
        .await
        .expect("source journal");
    let close: temper_server::entity_actor::EntityEvent = serde_json::from_value(
        events
            .iter()
            .find(|event| event.event_type == "Close")
            .expect("Close event")
            .payload
            .clone(),
    )
    .expect("Close payload");
    assert!(
        close
            .params
            .get("_temper_state_timeout_declaration_v1")
            .is_none(),
        "caller-supplied occurrence evidence must be stripped before commit"
    );
}
