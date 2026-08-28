use super::*;

use temper_runtime::persistence::schema_deployment::{
    CompleteSchemaBootstrap, RecordSchemaBootstrapCreated, ReserveSchemaBootstrap,
    ReserveSchemaBootstrapOutcome, SchemaBootstrapFailure, SchemaBootstrapFailureStage,
    SchemaBootstrapReceipt, SchemaBootstrapStatus,
};

fn bootstrap_command(
    caller_hex: char,
    idempotency_key: &str,
    request_digest: &str,
    activation_request_id: &str,
    entity_id: &str,
) -> ReserveSchemaBootstrap {
    ReserveSchemaBootstrap {
        tenant: "tenant-a".into(),
        caller_authority: format!("sha256:{}", caller_hex.to_string().repeat(64)),
        accepted_authority_json: r#"{"principal":"caller-a"}"#.into(),
        idempotency_key: idempotency_key.into(),
        request_digest: request_digest.into(),
        request_id: format!("request-{idempotency_key}"),
        activation_request_id: activation_request_id.into(),
        entity_type: "Example.Task".into(),
        entity_id: entity_id.into(),
        canonical_initial_fields_json: r#"{"Title":"first"}"#.into(),
        initial_action: None,
    }
}

async fn active_bootstrap_bundle(store: &SimEventStore) -> (String, SchemaActivePointer) {
    let digest = format!("sha256:{}", "a".repeat(64));
    store
        .submit_schema_bundle(command(
            "bootstrap-submit",
            &format!("sha256:{}", "1".repeat(64)),
            &digest,
        ))
        .await
        .unwrap();
    let fence = verify_bundle(store, "bootstrap", &digest, 1).await;
    let pointer = activated(
        store
            .activate_schema_bundle(activation_command(
                "bootstrap-activate",
                &digest,
                None,
                fence,
                "bootstrap-receipt",
            ))
            .await
            .unwrap(),
    );
    (digest, pointer)
}

#[tokio::test]
async fn bootstrap_reservation_replays_exact_receipt_and_fences_target_races() {
    let store = SimEventStore::no_faults(78);
    let (digest, pointer) = active_bootstrap_bundle(&store).await;
    let activation_request_id = "request-bootstrap-activate";
    assert_eq!(pointer.accepted_request_id, activation_request_id);

    let command = bootstrap_command(
        'a',
        "bootstrap-1",
        &format!("sha256:{}", "2".repeat(64)),
        activation_request_id,
        "entity-1",
    );
    store.fail_next_schema_operations(SimSchemaFaultPoint::ReserveBootstrap, 1);
    assert_injected_failure(
        store
            .reserve_schema_bootstrap(command.clone())
            .await
            .unwrap_err(),
        SimSchemaFaultPoint::ReserveBootstrap,
    );

    let reserved = match store
        .reserve_schema_bootstrap(command.clone())
        .await
        .unwrap()
    {
        ReserveSchemaBootstrapOutcome::Reserved(operation) => operation,
        ReserveSchemaBootstrapOutcome::Replayed(_) => panic!("first reservation must be new"),
    };
    assert_eq!(reserved.status, SchemaBootstrapStatus::Reserved);
    assert_eq!(reserved.pin.scope, scope());
    assert_eq!(reserved.pin.bundle_digest, digest);

    let replay = match store
        .reserve_schema_bootstrap(command.clone())
        .await
        .unwrap()
    {
        ReserveSchemaBootstrapOutcome::Replayed(operation) => operation,
        ReserveSchemaBootstrapOutcome::Reserved(_) => panic!("retry must replay"),
    };
    assert_eq!(replay, reserved);

    let mut conflicting_key = command.clone();
    conflicting_key.request_digest = format!("sha256:{}", "3".repeat(64));
    assert_eq!(
        store
            .reserve_schema_bootstrap(conflicting_key)
            .await
            .unwrap_err(),
        SchemaDeploymentStoreError::IdempotencyConflict
    );
    let target_race = bootstrap_command(
        'b',
        "bootstrap-2",
        &format!("sha256:{}", "4".repeat(64)),
        activation_request_id,
        "entity-1",
    );
    assert_eq!(
        store
            .reserve_schema_bootstrap(target_race)
            .await
            .unwrap_err(),
        SchemaDeploymentStoreError::BootstrapTargetConflict
    );

    store.fail_next_schema_operations(SimSchemaFaultPoint::RecordBootstrapCreated, 1);
    let created_command = RecordSchemaBootstrapCreated {
        tenant: "tenant-a".into(),
        caller_authority: format!("sha256:{}", "a".repeat(64)),
        idempotency_key: "bootstrap-1".into(),
        expected_sequence: reserved.committed_sequence,
        creation_sequence: 1,
    };
    assert_injected_failure(
        store
            .record_schema_bootstrap_created(created_command.clone())
            .await
            .unwrap_err(),
        SimSchemaFaultPoint::RecordBootstrapCreated,
    );
    let created = store
        .record_schema_bootstrap_created(created_command.clone())
        .await
        .unwrap();
    assert_eq!(created.status, SchemaBootstrapStatus::Created);
    assert_eq!(created.creation_sequence, Some(1));
    assert_eq!(
        store
            .record_schema_bootstrap_created(created_command)
            .await
            .unwrap(),
        created
    );

    let receipt = SchemaBootstrapReceipt {
        request_id: command.request_id.clone(),
        pin: created.pin.clone(),
        entity_type: command.entity_type.clone(),
        entity_id: command.entity_id.clone(),
        creation_sequence: Some(1),
        action_sequence: None,
        canonical_action_result_json: None,
        failure: Some(SchemaBootstrapFailure {
            stage: SchemaBootstrapFailureStage::Action,
            code: "guard_rejected".into(),
            message: "the initial action guard rejected".into(),
            retryable: false,
            decision_id: None,
            details: BTreeMap::new(),
        }),
    };
    store.fail_next_schema_operations(SimSchemaFaultPoint::CompleteBootstrap, 1);
    let complete = CompleteSchemaBootstrap {
        tenant: "tenant-a".into(),
        caller_authority: format!("sha256:{}", "a".repeat(64)),
        idempotency_key: "bootstrap-1".into(),
        expected_sequence: created.committed_sequence,
        receipt: receipt.clone(),
    };
    assert_injected_failure(
        store
            .complete_schema_bootstrap(complete.clone())
            .await
            .unwrap_err(),
        SimSchemaFaultPoint::CompleteBootstrap,
    );
    let completed = store
        .complete_schema_bootstrap(complete.clone())
        .await
        .unwrap();
    assert_eq!(completed.status, SchemaBootstrapStatus::Completed);
    assert_eq!(completed.receipt.as_ref(), Some(&receipt));
    assert_eq!(
        store.complete_schema_bootstrap(complete).await.unwrap(),
        completed
    );
    assert!(
        store
            .list_incomplete_schema_bootstraps(8)
            .await
            .unwrap()
            .is_empty()
    );
    let final_replay = match store.reserve_schema_bootstrap(command).await.unwrap() {
        ReserveSchemaBootstrapOutcome::Replayed(operation) => operation,
        ReserveSchemaBootstrapOutcome::Reserved(_) => panic!("completed retry must replay"),
    };
    assert_eq!(final_replay, completed);
}

#[tokio::test]
async fn bootstrap_reservation_requires_the_still_active_activation_identity() {
    let store = SimEventStore::no_faults(79);
    active_bootstrap_bundle(&store).await;
    let command = bootstrap_command(
        'a',
        "bootstrap-stale",
        &format!("sha256:{}", "5".repeat(64)),
        "request-not-active",
        "entity-2",
    );
    assert_eq!(
        store.reserve_schema_bootstrap(command).await.unwrap_err(),
        SchemaDeploymentStoreError::NotFound
    );
}
