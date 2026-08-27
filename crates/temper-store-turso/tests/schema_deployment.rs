use std::collections::BTreeMap;

use temper_runtime::persistence::schema_deployment::{
    ActivateSchemaBundle, ActivateSchemaBundleOutcome, ClaimSchemaVerification,
    ClaimSchemaVerificationOutcome, CommitSchemaMigrationBatch, CreateSchemaMigration,
    ReserveSchemaMigrationRetry, RetireSchemaBundle, RetireSchemaBundleOutcome,
    SchemaActivePointer, SchemaBundleRecord, SchemaDeploymentRecord, SchemaDeploymentStatus,
    SchemaDeploymentStore, SchemaDeploymentStoreError, SchemaExecutionPin,
    SchemaMigrationBatchReceipt, SchemaMigrationBudgets, SchemaMigrationShadowRow,
    SchemaMigrationStatus, SchemaMigrationValidationReceipt, SchemaOperationIdentity, SchemaScope,
    SchemaScopeKind, SchemaVerificationReceipt, SubmitSchemaBundle, SubmitSchemaBundleOutcome,
};
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_store_turso::TursoEventStore;

fn scope() -> SchemaScope {
    SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-contract".into(),
    }
}

fn command(key: &str, request_digest: &str, digest: &str) -> SubmitSchemaBundle {
    SubmitSchemaBundle {
        bundle: SchemaBundleRecord {
            tenant: "tenant-contract".into(),
            scope: scope(),
            digest: digest.into(),
            predecessor_digest: None,
            canonical_csdl: "<canonical/>".into(),
            canonical_ioa: BTreeMap::from([("Example.Task".into(), "[automaton]".into())]),
            cedar_policies: BTreeMap::new(),
            wasm_module_digests: BTreeMap::new(),
            migration_module_name: None,
            migration_module_digest: None,
            migration_abi_version: None,
            canonical_budgets: "verification_steps=100".into(),
        },
        idempotency_key: key.into(),
        request_digest: request_digest.into(),
        request_id: format!("request-{key}"),
    }
}

fn operation(key: &str) -> SchemaOperationIdentity {
    SchemaOperationIdentity {
        idempotency_key: key.into(),
        request_digest: format!("sha256:{}", "d".repeat(64)),
        request_id: format!("request-{key}"),
    }
}

fn claim_command(key: &str, digest: &str, now: u64, lease: u64) -> ClaimSchemaVerification {
    ClaimSchemaVerification {
        tenant: "tenant-contract".into(),
        scope: scope(),
        bundle_digest: digest.into(),
        logical_now: now,
        lease_expires_at: lease,
        operation: operation(key),
    }
}

fn claimed(outcome: ClaimSchemaVerificationOutcome) -> SchemaDeploymentRecord {
    match outcome {
        ClaimSchemaVerificationOutcome::Claimed(record)
        | ClaimSchemaVerificationOutcome::Replayed(record) => record,
    }
}

fn activation_command(
    key: &str,
    digest: &str,
    predecessor: Option<&str>,
    fence: u64,
    receipt_id: &str,
) -> ActivateSchemaBundle {
    ActivateSchemaBundle {
        tenant: "tenant-contract".into(),
        scope: scope(),
        bundle_digest: digest.into(),
        expected_predecessor: predecessor.map(str::to_string),
        expected_fence: fence,
        verification_receipt_id: receipt_id.into(),
        stream_publication_fence: None,
        operation: operation(key),
    }
}

fn activated(outcome: ActivateSchemaBundleOutcome) -> SchemaActivePointer {
    match outcome {
        ActivateSchemaBundleOutcome::Activated(pointer)
        | ActivateSchemaBundleOutcome::Replayed(pointer) => pointer,
    }
}

fn retire_command(key: &str, digest: &str, fence: u64) -> RetireSchemaBundle {
    RetireSchemaBundle {
        tenant: "tenant-contract".into(),
        scope: scope(),
        bundle_digest: digest.into(),
        expected_fence: fence,
        operation: operation(key),
    }
}

fn retired(outcome: RetireSchemaBundleOutcome) -> SchemaDeploymentRecord {
    match outcome {
        RetireSchemaBundleOutcome::Retired(record)
        | RetireSchemaBundleOutcome::Replayed(record) => record,
    }
}

#[tokio::test]
async fn turso_schema_deployment_core_contract() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("file:{}", directory.path().join("schema.db").display());
    let store = TursoEventStore::new(&url, None).await.unwrap();
    let digest = format!("sha256:{}", "a".repeat(64));
    let request = format!("sha256:{}", "b".repeat(64));
    let missing_digest = format!("sha256:{}", "f".repeat(64));
    assert!(matches!(
        store
            .submit_schema_bundle(command(
                "invalid-digest",
                &request,
                &format!("sha256:{}", "A".repeat(64)),
            ))
            .await,
        Err(SchemaDeploymentStoreError::InvalidInput(_))
    ));

    assert!(matches!(
        store
            .submit_schema_bundle(command("submit-1", &request, &digest))
            .await
            .unwrap(),
        SubmitSchemaBundleOutcome::Created(_)
    ));
    assert!(matches!(
        store
            .submit_schema_bundle(command("submit-1", &request, &digest))
            .await
            .unwrap(),
        SubmitSchemaBundleOutcome::Replayed(_)
    ));
    assert_eq!(
        store
            .submit_schema_bundle(command(
                "submit-1",
                &format!("sha256:{}", "c".repeat(64)),
                &digest,
            ))
            .await
            .unwrap_err(),
        SchemaDeploymentStoreError::IdempotencyConflict
    );

    let first = claimed(
        store
            .claim_schema_verification(claim_command("verify-1", &digest, 10, 20))
            .await
            .unwrap(),
    );
    let second = claimed(
        store
            .claim_schema_verification(claim_command("verify-2", &digest, 20, 30))
            .await
            .unwrap(),
    );
    assert!(second.fence > first.fence);
    assert_eq!(
        store
            .finish_schema_verification(
                "tenant-contract",
                &scope(),
                &digest,
                first.fence,
                SchemaVerificationReceipt {
                    id: "stale".into(),
                    verifier_version: "v1".into(),
                    input_digest: request.clone(),
                    passed: true,
                },
            )
            .await
            .unwrap_err(),
        SchemaDeploymentStoreError::StaleFence
    );
    let verified = store
        .finish_schema_verification(
            "tenant-contract",
            &scope(),
            &digest,
            second.fence,
            SchemaVerificationReceipt {
                id: "verify-1".into(),
                verifier_version: "v1".into(),
                input_digest: request,
                passed: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(verified.status, SchemaDeploymentStatus::Verified);
    assert!(matches!(
        store
            .claim_schema_verification(claim_command("verify-2", &digest, 21, 31))
            .await
            .unwrap(),
        ClaimSchemaVerificationOutcome::Replayed(record)
            if record.status == SchemaDeploymentStatus::Verified
    ));
    assert_eq!(
        store
            .activate_schema_bundle(activation_command(
                "activate-stale",
                &digest,
                Some(&missing_digest),
                verified.fence,
                "verify-1",
            ))
            .await
            .unwrap_err(),
        SchemaDeploymentStoreError::PredecessorMismatch
    );
    assert!(
        store
            .active_schema_pointer("tenant-contract", &scope())
            .await
            .unwrap()
            .is_none()
    );
    let activation = activation_command("activate-1", &digest, None, verified.fence, "verify-1");
    let pointer = activated(
        store
            .activate_schema_bundle(activation.clone())
            .await
            .unwrap(),
    );
    assert!(matches!(
        store.activate_schema_bundle(activation).await.unwrap(),
        ActivateSchemaBundleOutcome::Replayed(_)
    ));
    let mut conflicting_activation =
        activation_command("activate-1", &digest, None, verified.fence, "verify-1");
    conflicting_activation.operation.request_digest = format!("sha256:{}", "e".repeat(64));
    assert_eq!(
        store
            .activate_schema_bundle(conflicting_activation)
            .await
            .unwrap_err(),
        SchemaDeploymentStoreError::IdempotencyConflict
    );
    assert_eq!(
        store
            .active_schema_pointer("tenant-contract", &scope())
            .await
            .unwrap(),
        Some(pointer.clone())
    );
    let pin = SchemaExecutionPin {
        scope: scope(),
        bundle_digest: digest.clone(),
    };
    let inactive_pin = SchemaExecutionPin {
        scope: SchemaScope {
            kind: SchemaScopeKind::Task,
            id: "inactive-same-digest".into(),
        },
        bundle_digest: digest.clone(),
    };
    let inactive_id = format!(
        "tenant-contract:Task:{}",
        temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
            "cross-scope",
            &inactive_pin,
        )
    );
    assert!(
        store
            .append(
                &inactive_id,
                0,
                &[PersistenceEnvelope {
                    sequence_nr: 1,
                    event_type: "Configure".into(),
                    payload: serde_json::json!({}),
                    metadata: EventMetadata {
                        event_id: temper_runtime::scheduler::sim_uuid(),
                        causation_id: temper_runtime::scheduler::sim_uuid(),
                        correlation_id: temper_runtime::scheduler::sim_uuid(),
                        timestamp: temper_runtime::scheduler::sim_now(),
                        actor_id: "cross-scope-fence-contract".into(),
                        kernel: None,
                    },
                }],
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("stale scoped schema write fence")
    );
    let pinned_id = format!(
        "tenant-contract:Task:{}",
        temper_runtime::persistence::schema_deployment::scoped_journal_entity_id("entity-雪", &pin,)
    );
    store
        .append(
            &pinned_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Configure".into(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: temper_runtime::scheduler::sim_uuid(),
                    causation_id: temper_runtime::scheduler::sim_uuid(),
                    correlation_id: temper_runtime::scheduler::sim_uuid(),
                    timestamp: temper_runtime::scheduler::sim_now(),
                    actor_id: "pin-contract".into(),
                    kernel: None,
                },
            }],
        )
        .await
        .unwrap();
    let collision_base = "entity-collision";
    let collision_entity = temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
        collision_base,
        &pin,
    );
    store
        .append(
            &format!(
                "tenant-contract:Task:{}",
                temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                    &collision_entity,
                    &pin,
                )
            ),
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Configure".into(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: temper_runtime::scheduler::sim_uuid(),
                    causation_id: temper_runtime::scheduler::sim_uuid(),
                    correlation_id: temper_runtime::scheduler::sim_uuid(),
                    timestamp: temper_runtime::scheduler::sim_now(),
                    actor_id: "pin-collision-contract".into(),
                    kernel: None,
                },
            }],
        )
        .await
        .unwrap();
    assert!(
        store
            .scoped_entity_bundle_digests("tenant-contract", "Task", collision_base, &scope(), 2,)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .scoped_entity_bundle_digests(
                "tenant-contract",
                "Task",
                &collision_entity,
                &scope(),
                2,
            )
            .await
            .unwrap(),
        vec![digest.clone()]
    );
    assert_eq!(
        store
            .scoped_entity_bundle_digests("tenant-contract", "Task", "entity-雪", &scope(), 2,)
            .await
            .unwrap(),
        vec![digest.clone()]
    );
    assert_eq!(
        store
            .retire_schema_bundle(retire_command("retire-stale", &digest, pointer.fence - 1))
            .await
            .unwrap_err(),
        SchemaDeploymentStoreError::StaleFence
    );
    let retirement = retire_command("retire-1", &digest, pointer.fence);
    let retired = retired(
        store
            .retire_schema_bundle(retirement.clone())
            .await
            .unwrap(),
    );
    assert!(matches!(
        store.retire_schema_bundle(retirement).await.unwrap(),
        RetireSchemaBundleOutcome::Replayed(_)
    ));
    let mut conflicting_retirement = retire_command("retire-1", &digest, pointer.fence);
    conflicting_retirement.operation.request_digest = format!("sha256:{}", "e".repeat(64));
    assert_eq!(
        store
            .retire_schema_bundle(conflicting_retirement)
            .await
            .unwrap_err(),
        SchemaDeploymentStoreError::IdempotencyConflict
    );
    assert_eq!(retired.status, SchemaDeploymentStatus::Retired);
    assert!(
        store
            .active_schema_pointer("tenant-contract", &scope())
            .await
            .unwrap()
            .is_none()
    );
}

async fn verified(
    store: &TursoEventStore,
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
            "tenant-contract",
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
async fn turso_schema_migration_contract() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("file:{}", directory.path().join("migration.db").display());
    let store = TursoEventStore::new(&url, None).await.unwrap();
    let source_digest = format!("sha256:{}", "1".repeat(64));
    let source_request = format!("sha256:{}", "2".repeat(64));
    store
        .submit_schema_bundle(command("source", &source_request, &source_digest))
        .await
        .unwrap();
    let source = verified(&store, &source_digest, &source_request, "source-verify").await;
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

    let target_digest = format!("sha256:{}", "3".repeat(64));
    let target_request = format!("sha256:{}", "4".repeat(64));
    let module_digest = format!("sha256:{}", "5".repeat(64));
    let mut target = command("target", &target_request, &target_digest);
    target.bundle.predecessor_digest = Some(source_digest.clone());
    target.bundle.migration_module_name = Some("reshape".into());
    target.bundle.migration_module_digest = Some(module_digest.clone());
    target.bundle.migration_abi_version = Some("temper-schema-migration/v1".into());
    store.submit_schema_bundle(target).await.unwrap();
    verified(&store, &target_digest, &target_request, "target-verify").await;

    store
        .create_schema_migration(CreateSchemaMigration {
            job_id: "migration-1".into(),
            tenant: "tenant-contract".into(),
            scope: scope(),
            source_bundle_digest: source_digest.clone(),
            target_bundle_digest: target_digest.clone(),
            verification_receipt_id: "target-verify".into(),
            source_expected_fence: source_pointer.fence,
            module_name: "reshape".into(),
            module_digest,
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
            request_digest: format!("sha256:{}", "6".repeat(64)),
            request_id: "migration-request".into(),
        })
        .await
        .unwrap();
    let retry = ReserveSchemaMigrationRetry {
        tenant: "tenant-contract".into(),
        job_id: "migration-1".into(),
        operation: operation("migration-retry-1"),
    };
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
    let claim = store
        .claim_schema_migration("tenant-contract", "migration-1", 10, 20)
        .await
        .unwrap();
    let row = SchemaMigrationShadowRow {
        entity_type: "Example.Task".into(),
        entity_id: "task-1".into(),
        source_sequence: 4,
        canonical_state_json: r#"{"Id":"task-1","Status":"Open"}"#.into(),
        input_digest: format!("sha256:{}", "7".repeat(64)),
        output_digest: format!("sha256:{}", "8".repeat(64)),
        target_event: PersistenceEnvelope {
            sequence_nr: 1,
            event_type: "FieldUpdate".into(),
            payload: serde_json::json!({}),
            metadata: EventMetadata {
                event_id: temper_runtime::scheduler::sim_uuid(),
                causation_id: temper_runtime::scheduler::sim_uuid(),
                correlation_id: temper_runtime::scheduler::sim_uuid(),
                timestamp: temper_runtime::scheduler::sim_now(),
                actor_id: "migration-test".into(),
                kernel: None,
            },
        },
    };
    let cursor = Some((row.entity_type.clone(), row.entity_id.clone()));
    let command = CommitSchemaMigrationBatch {
        job_id: "migration-1".into(),
        expected_fence: claim.fence,
        expected_cursor: None,
        next_cursor: cursor.clone(),
        scan_complete: true,
        restart_scan: false,
        observed_source_write_version: 0,
        rows: vec![row.clone()],
        receipt: SchemaMigrationBatchReceipt {
            id: "batch-1".into(),
            source_cursor: None,
            next_cursor: cursor,
            input_digest: row.input_digest.clone(),
            output_digest: row.output_digest.clone(),
            row_count: 1,
        },
    };
    let validating = store
        .commit_schema_migration_batch("tenant-contract", command.clone())
        .await
        .unwrap();
    assert_eq!(validating.status, SchemaMigrationStatus::Validating);
    assert_eq!(
        store
            .commit_schema_migration_batch("tenant-contract", command)
            .await
            .unwrap(),
        validating
    );
    let ready = store
        .validate_schema_migration(
            "tenant-contract",
            "migration-1",
            validating.fence,
            SchemaMigrationValidationReceipt {
                id: "validation-1".into(),
                shadow_digest: row.output_digest.clone(),
                caught_up_sequence: 0,
                passed: true,
            },
        )
        .await
        .unwrap();
    let pointer = store
        .cut_over_schema_migration(
            "tenant-contract",
            "migration-1",
            ready.fence,
            "validation-1",
        )
        .await
        .unwrap();
    assert_eq!(pointer.bundle_digest, target_digest);
    assert_eq!(pointer.predecessor_digest, Some(source_digest));
    assert_eq!(
        store
            .page_schema_migration_shadow("tenant-contract", "migration-1", None, 2)
            .await
            .unwrap(),
        vec![row]
    );
}
