use super::*;

async fn verify(
    store: &SimEventStore,
    digest: &str,
    request_digest: &str,
    receipt_id: &str,
) -> temper_runtime::persistence::schema_deployment::SchemaDeploymentRecord {
    let claim = claimed(
        store
            .claim_schema_verification(claim_command(receipt_id, digest, 1, 10))
            .await
            .unwrap(),
    );
    store
        .finish_schema_verification(
            "tenant-a",
            &scope(),
            digest,
            claim.fence,
            SchemaVerificationReceipt {
                id: receipt_id.into(),
                verifier_version: "v1".into(),
                input_digest: request_digest.into(),
                passed: true,
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn migration_batches_replay_and_cut_over_atomically() {
    let store = SimEventStore::no_faults(10);
    let source_digest = format!("sha256:{}", "2".repeat(64));
    let source_request = format!("sha256:{}", "3".repeat(64));
    store
        .submit_schema_bundle(command("source-submit", &source_request, &source_digest))
        .await
        .unwrap();
    let source = verify(&store, &source_digest, &source_request, "source-verify").await;
    let source_pointer = activated(
        store
            .activate_schema_bundle(activation_command(
                "source-activate",
                &source_digest,
                None,
                source.fence,
                "source-verify",
            ))
            .await
            .unwrap(),
    );

    let target_digest = format!("sha256:{}", "4".repeat(64));
    let target_request = format!("sha256:{}", "5".repeat(64));
    let mut target_command = command("target-submit", &target_request, &target_digest);
    target_command.bundle.predecessor_digest = Some(source_digest.clone());
    target_command.bundle.migration_module_name = Some("reshape".into());
    target_command.bundle.migration_module_digest = Some(format!("sha256:{}", "6".repeat(64)));
    target_command.bundle.migration_abi_version = Some("temper-schema-migration/v1".into());
    store.submit_schema_bundle(target_command).await.unwrap();
    verify(&store, &target_digest, &target_request, "target-verify").await;

    let create = CreateSchemaMigration {
        job_id: "migration-1".into(),
        tenant: "tenant-a".into(),
        scope: scope(),
        source_bundle_digest: source_digest.clone(),
        target_bundle_digest: target_digest.clone(),
        verification_receipt_id: "target-verify".into(),
        source_expected_fence: source_pointer.fence,
        module_name: "reshape".into(),
        module_digest: format!("sha256:{}", "6".repeat(64)),
        accepted_authority_json: r#"{"principal":"agent-a"}"#.into(),
        budgets: SchemaMigrationBudgets {
            fuel_per_entity: 10_000,
            memory_pages: 2,
            input_bytes: 4_096,
            output_bytes: 4_096,
            entities_per_batch: 2,
            total_entities: 10,
            total_batches: 5,
            attempts: 3,
        },
        idempotency_key: "migration-key".into(),
        request_digest: format!("sha256:{}", "7".repeat(64)),
        request_id: "migration-request".into(),
    };
    store.fail_next_schema_operations(SimSchemaFaultPoint::CreateMigration, 1);
    let failure = store
        .create_schema_migration(create.clone())
        .await
        .unwrap_err();
    assert_injected_failure(failure, SimSchemaFaultPoint::CreateMigration);
    assert!(
        store
            .get_schema_migration("tenant-a", "migration-1")
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        store.create_schema_migration(create.clone()).await.unwrap(),
        CreateSchemaMigrationOutcome::Created(_)
    ));
    assert!(matches!(
        store.create_schema_migration(create.clone()).await.unwrap(),
        CreateSchemaMigrationOutcome::Replayed(_)
    ));
    let mut rejected_create = create.clone();
    rejected_create.job_id = "migration-rejected".into();
    rejected_create.idempotency_key = "migration-rejected-key".into();
    rejected_create.request_digest = format!("sha256:{}", "a".repeat(64));
    rejected_create.request_id = "migration-rejected-request".into();
    store
        .create_schema_migration(rejected_create)
        .await
        .unwrap();
    let rejected_claim = store
        .claim_schema_migration("tenant-a", "migration-rejected", 10, 20)
        .await
        .unwrap();
    let rejection = SchemaMigrationValidationReceipt {
        id: "rejection-receipt".into(),
        shadow_digest: format!("sha256:{}", "b".repeat(64)),
        caught_up_sequence: rejected_claim.catch_up_sequence,
        passed: false,
    };
    let rejected = store
        .validate_schema_migration(
            "tenant-a",
            "migration-rejected",
            rejected_claim.fence,
            rejection.clone(),
        )
        .await
        .unwrap();
    assert_eq!(rejected.status, SchemaMigrationStatus::Rejected);
    assert_eq!(
        rejected.migration_receipt_id.as_deref(),
        Some("migration-rejected:migration-rejected")
    );
    assert_eq!(
        store
            .validate_schema_migration(
                "tenant-a",
                "migration-rejected",
                rejected_claim.fence,
                rejection,
            )
            .await
            .unwrap(),
        rejected
    );
    let retry = ReserveSchemaMigrationRetry {
        tenant: "tenant-a".into(),
        job_id: "migration-1".into(),
        operation: operation("migration-retry-1"),
    };
    store.fail_next_schema_operations(SimSchemaFaultPoint::ReserveMigrationRetry, 1);
    let failure = store
        .reserve_schema_migration_retry(retry.clone())
        .await
        .unwrap_err();
    assert_injected_failure(failure, SimSchemaFaultPoint::ReserveMigrationRetry);
    let reserved = store
        .reserve_schema_migration_retry(retry.clone())
        .await
        .unwrap();
    assert!(!reserved.replayed);
    assert_eq!(reserved.starting_sequence, 1);
    assert!(
        store
            .reserve_schema_migration_retry(retry)
            .await
            .unwrap()
            .replayed
    );
    let mut conflicting_retry = ReserveSchemaMigrationRetry {
        tenant: "tenant-a".into(),
        job_id: "migration-1".into(),
        operation: operation("migration-retry-1"),
    };
    conflicting_retry.operation.request_digest = format!("sha256:{}", "e".repeat(64));
    assert_eq!(
        store
            .reserve_schema_migration_retry(conflicting_retry)
            .await
            .unwrap_err(),
        SchemaDeploymentStoreError::IdempotencyConflict
    );
    store.fail_next_schema_operations(SimSchemaFaultPoint::ClaimMigration, 1);
    let failure = store
        .claim_schema_migration("tenant-a", "migration-1", 10, 20)
        .await
        .unwrap_err();
    assert_injected_failure(failure, SimSchemaFaultPoint::ClaimMigration);
    assert_eq!(
        store
            .get_schema_migration("tenant-a", "migration-1")
            .await
            .unwrap()
            .unwrap()
            .status,
        SchemaMigrationStatus::Submitted
    );
    let claim = store
        .claim_schema_migration("tenant-a", "migration-1", 10, 20)
        .await
        .unwrap();
    let row = SchemaMigrationShadowRow {
        entity_type: "Example.Task".into(),
        entity_id: "task-1".into(),
        source_sequence: 4,
        canonical_state_json: r#"{"Id":"task-1","Status":"Open"}"#.into(),
        input_digest: format!("sha256:{}", "8".repeat(64)),
        output_digest: format!("sha256:{}", "9".repeat(64)),
        target_event: PersistenceEnvelope {
            sequence_nr: 1,
            event_type: "FieldUpdate".into(),
            payload: serde_json::json!({}),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: "migration-test".into(),
            },
        },
    };
    let receipt = SchemaMigrationBatchReceipt {
        id: "batch-1".into(),
        source_cursor: None,
        next_cursor: Some(("Example.Task".into(), "task-1".into())),
        input_digest: row.input_digest.clone(),
        output_digest: row.output_digest.clone(),
        row_count: 1,
    };
    let batch = CommitSchemaMigrationBatch {
        job_id: "migration-1".into(),
        expected_fence: claim.fence,
        expected_cursor: None,
        next_cursor: receipt.next_cursor.clone(),
        scan_complete: true,
        restart_scan: false,
        observed_source_write_version: 0,
        rows: vec![row.clone()],
        receipt,
    };
    store.fail_next_schema_operations(SimSchemaFaultPoint::CommitMigrationBatch, 1);
    let failure = store
        .commit_schema_migration_batch("tenant-a", batch.clone())
        .await
        .unwrap_err();
    assert_injected_failure(failure, SimSchemaFaultPoint::CommitMigrationBatch);
    assert!(
        store
            .page_schema_migration_shadow("tenant-a", "migration-1", None, 2)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        temper_runtime::persistence::EventStore::read_events(
            &store,
            &scoped_persistence_id("Example.Task", "task-1", &target_digest),
            0,
        )
        .await
        .unwrap()
        .is_empty(),
        "a failed batch must not expose a target journal"
    );
    assert_eq!(
        store
            .get_schema_migration("tenant-a", "migration-1")
            .await
            .unwrap()
            .unwrap()
            .status,
        SchemaMigrationStatus::Migrating
    );
    store.fail_next_schema_operations(SimSchemaFaultPoint::CommitMigrationBatchResponseLoss, 1);
    let lost_response = store
        .commit_schema_migration_batch("tenant-a", batch.clone())
        .await
        .unwrap_err();
    assert_injected_failure(
        lost_response,
        SimSchemaFaultPoint::CommitMigrationBatchResponseLoss,
    );
    assert_eq!(
        temper_runtime::persistence::EventStore::read_events(
            &store,
            &scoped_persistence_id("Example.Task", "task-1", &target_digest),
            0,
        )
        .await
        .unwrap()
        .len(),
        1,
        "response loss must retain the atomically committed target event"
    );
    let validating = store
        .commit_schema_migration_batch("tenant-a", batch.clone())
        .await
        .unwrap();
    assert_eq!(validating.status, SchemaMigrationStatus::Validating);
    assert_eq!(
        store
            .commit_schema_migration_batch("tenant-a", batch)
            .await
            .unwrap(),
        validating
    );
    let validation = SchemaMigrationValidationReceipt {
        id: "validation-1".into(),
        shadow_digest: row.output_digest.clone(),
        caught_up_sequence: 0,
        passed: true,
    };
    store.fail_next_schema_operations(SimSchemaFaultPoint::ValidateMigration, 1);
    let failure = store
        .validate_schema_migration(
            "tenant-a",
            "migration-1",
            validating.fence,
            validation.clone(),
        )
        .await
        .unwrap_err();
    assert_injected_failure(failure, SimSchemaFaultPoint::ValidateMigration);
    assert_eq!(
        store
            .get_schema_migration("tenant-a", "migration-1")
            .await
            .unwrap()
            .unwrap()
            .status,
        SchemaMigrationStatus::Validating
    );
    let ready = store
        .validate_schema_migration("tenant-a", "migration-1", validating.fence, validation)
        .await
        .unwrap();
    assert_eq!(ready.status, SchemaMigrationStatus::Ready);
    store.fail_next_schema_operations(SimSchemaFaultPoint::CutOverMigration, 1);
    let failure = store
        .cut_over_schema_migration("tenant-a", "migration-1", ready.fence, "validation-1")
        .await
        .unwrap_err();
    assert_injected_failure(failure, SimSchemaFaultPoint::CutOverMigration);
    assert_eq!(
        store
            .active_schema_pointer("tenant-a", &scope())
            .await
            .unwrap(),
        Some(source_pointer.clone())
    );
    let target_pointer = store
        .cut_over_schema_migration("tenant-a", "migration-1", ready.fence, "validation-1")
        .await
        .unwrap();
    assert_eq!(target_pointer.bundle_digest, target_digest);
    assert_eq!(target_pointer.predecessor_digest, Some(source_digest));
    store.fail_next_schema_operations(SimSchemaFaultPoint::CompleteMigration, 1);
    let failure = store
        .complete_schema_migration("tenant-a", "migration-1", ready.fence)
        .await
        .unwrap_err();
    assert_injected_failure(failure, SimSchemaFaultPoint::CompleteMigration);
    assert_eq!(
        store
            .get_schema_migration("tenant-a", "migration-1")
            .await
            .unwrap()
            .unwrap()
            .status,
        SchemaMigrationStatus::CutOver
    );
    let completed = store
        .complete_schema_migration("tenant-a", "migration-1", ready.fence)
        .await
        .unwrap();
    assert_eq!(completed.status, SchemaMigrationStatus::Completed);
    assert_eq!(
        store
            .page_schema_migration_shadow("tenant-a", "migration-1", None, 2)
            .await
            .unwrap(),
        vec![row]
    );

    let event_id = sim_uuid();
    let envelope = PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Update".into(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id,
            causation_id: event_id,
            correlation_id: event_id,
            timestamp: sim_now(),
            actor_id: "test".into(),
        },
    };
    let stale_source_id = scoped_persistence_id(
        "Example.Task",
        "task-1",
        target_pointer.predecessor_digest.as_deref().unwrap(),
    );
    assert!(
        EventStore::append(&store, &stale_source_id, 0, std::slice::from_ref(&envelope))
            .await
            .is_err()
    );
    let active_target_id =
        scoped_persistence_id("Example.Task", "task-1", &target_pointer.bundle_digest);
    EventStore::append(&store, &active_target_id, 1, &[envelope])
        .await
        .unwrap();
}
