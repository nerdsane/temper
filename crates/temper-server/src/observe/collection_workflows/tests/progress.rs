use super::*;

#[tokio::test]
async fn admitted_and_retrying_progress_is_derived_from_durable_delivery_after_restart() {
    let (state, store, _temp) = state().await;
    permit_reader(&state, "tenant-a");
    let mut record = workflow("tenant-a", "batch-a", &["private-member"]);
    let intent = seed_activated(&state, &mut record).await;

    let admitted = handle_get_workflow(
        State(state),
        Some(context("tenant-a", "reader")),
        Path(record.workflow_id.clone()),
    )
    .await
    .expect("inferred admitted delivery");
    let admitted = serde_json::to_value(admitted.0).expect("serialize admitted progress");
    assert_eq!(admitted["members"][0]["attempts"], 0);
    assert_eq!(admitted["members"][0]["delivery_class"], "pending");
    assert!(admitted["oldest_active_age_ms"].is_number());

    let boxed = BoxedEventStore::new(store.clone());
    let (mut delivery, sequence) = load_delivery_record(&boxed, intent)
        .await
        .expect("load inferred delivery");
    let now = temper_runtime::scheduler::sim_now();
    delivery
        .claim(now, chrono::Duration::seconds(30))
        .expect("claim first attempt");
    delivery.status = ReactionDeliveryStatus::Pending;
    delivery.lease_expires_at = None;
    delivery.next_attempt_at = Some(now + chrono::Duration::seconds(1));
    append_delivery_record(&boxed, sequence, &delivery)
        .await
        .expect("persist retry state");

    let mut restarted = ServerState::from_registry(
        ActorSystem::new("collection-observe-retry-restarted"),
        SpecRegistry::new(),
    );
    restarted.set_storage_stack(StorageStack::from_turso(store));
    permit_reader(&restarted, "tenant-a");
    let retrying = handle_get_workflow(
        State(restarted),
        Some(context("tenant-a", "reader")),
        Path(record.workflow_id),
    )
    .await
    .expect("retry state after restart");
    let retrying = serde_json::to_value(retrying.0).expect("serialize retry progress");
    assert_eq!(retrying["members"][0]["attempts"], 1);
    assert_eq!(retrying["total_attempts"], 1);
    assert_eq!(retrying["members"][0]["delivery_class"], "pending");
    assert!(retrying["oldest_active_age_ms"].is_number());
    assert!(!retrying.to_string().contains("private-member"));
}

#[tokio::test]
async fn cancellation_progress_preserves_member_attempt_accounting() {
    let (state, _store, _temp) = state().await;
    permit_reader(&state, "tenant-a");
    let mut record = workflow("tenant-a", "batch-a", &["private-member"]);
    let member_intents = activate_start(&mut record, 0, &actions()).expect("activate member");
    let member_id = record.members[0].member_id.clone();
    let member_delivery_id = record.members[0]
        .delivery_id
        .clone()
        .expect("member delivery");
    record
        .record_member_receipt(
            &member_id,
            &member_delivery_id,
            record.control_epoch,
            2,
            CollectionMemberReceipt {
                delivery_id: member_delivery_id.clone(),
                fencing_token: 2,
            },
        )
        .expect("record original member attempts");
    record
        .request_control(
            CollectionRequestedOutcome::Cancelled,
            None,
            "CancelChecks".to_string(),
            2,
            serde_json::json!({"principal": {"id": "secret-controller"}}),
            None,
        )
        .expect("request cancellation");
    let cancellation_intents = recover_progress(&mut record, 1).expect("bind cancellation");
    assert!(record.members[0].cancellation_delivery_id.is_some());

    let (store, _) = state.event_journal().expect("event journal");
    let mut append =
        workflow_append(&record, 0, "CollectionWorkflow::ControlledV1").expect("workflow append");
    let intents = member_intents
        .into_iter()
        .chain(cancellation_intents)
        .collect::<Vec<_>>();
    attach_intents(&mut append.events[0].payload, &intents).expect("attach delivery intents");
    store
        .append_batch(&[append])
        .await
        .expect("persist controlled workflow");

    let detail = handle_get_workflow(
        State(state),
        Some(context("tenant-a", "reader")),
        Path(record.workflow_id),
    )
    .await
    .expect("cancellation progress");
    let detail = serde_json::to_value(detail.0).expect("serialize cancellation progress");
    assert_eq!(detail["members"][0]["attempts"], 2);
    assert_eq!(detail["total_attempts"], 2);
    assert_eq!(detail["members"][0]["delivery_class"], "pending");
    assert!(detail["oldest_active_age_ms"].is_number());
}
