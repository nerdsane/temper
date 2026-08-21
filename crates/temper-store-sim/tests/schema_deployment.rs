use std::collections::BTreeMap;

use temper_runtime::persistence::schema_deployment::{
    ActivateSchemaBundle, ActivateSchemaBundleOutcome, ClaimSchemaVerification,
    ClaimSchemaVerificationOutcome, CommitSchemaMigrationBatch, CreateSchemaMigration,
    CreateSchemaMigrationOutcome, ReserveSchemaMigrationRetry, RetireSchemaBundle,
    RetireSchemaBundleOutcome, SchemaActivePointer, SchemaBundleRecord, SchemaDeploymentRecord,
    SchemaDeploymentStatus, SchemaDeploymentStore, SchemaDeploymentStoreError,
    SchemaMigrationBatchReceipt, SchemaMigrationBudgets, SchemaMigrationShadowRow,
    SchemaMigrationStatus, SchemaMigrationValidationReceipt, SchemaOperationIdentity, SchemaScope,
    SchemaScopeKind, SchemaVerificationReceipt, SubmitSchemaBundle, SubmitSchemaBundleOutcome,
};
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_store_sim::{SimEventStore, SimSchemaFaultPoint};

fn assert_injected_failure(error: SchemaDeploymentStoreError, point: SimSchemaFaultPoint) {
    assert!(
        matches!(
            error,
            SchemaDeploymentStoreError::BackendUnavailable(message)
                if message.contains(&format!("{point:?}"))
        ),
        "expected injected failure at {point:?}"
    );
}

fn scope() -> SchemaScope {
    SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-42".into(),
    }
}

fn command(key: &str, request_digest: &str, digest: &str) -> SubmitSchemaBundle {
    command_with_predecessor(key, request_digest, digest, None)
}

fn command_with_predecessor(
    key: &str,
    request_digest: &str,
    digest: &str,
    predecessor: Option<&str>,
) -> SubmitSchemaBundle {
    SubmitSchemaBundle {
        bundle: SchemaBundleRecord {
            tenant: "tenant-a".into(),
            scope: scope(),
            digest: digest.into(),
            predecessor_digest: predecessor.map(str::to_string),
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

fn test_event(sequence_nr: u64) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr,
        event_type: "Configure".into(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: "pin-write-fence-contract".into(),
        },
    }
}

fn scoped_persistence_id(entity_type: &str, entity_id: &str, digest: &str) -> String {
    scoped_persistence_id_in_scope(entity_type, entity_id, &scope(), digest)
}

fn scoped_persistence_id_in_scope(
    entity_type: &str,
    entity_id: &str,
    pin_scope: &SchemaScope,
    digest: &str,
) -> String {
    format!(
        "tenant-a:{entity_type}:{}",
        temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
            entity_id,
            &temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
                scope: pin_scope.clone(),
                bundle_digest: digest.to_string(),
            },
        )
    )
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
        tenant: "tenant-a".into(),
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
        tenant: "tenant-a".into(),
        scope: scope(),
        bundle_digest: digest.into(),
        expected_predecessor: predecessor.map(str::to_string),
        expected_fence: fence,
        verification_receipt_id: receipt_id.into(),
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
        tenant: "tenant-a".into(),
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

async fn verify_bundle(store: &SimEventStore, key: &str, digest: &str, logical_now: u64) -> u64 {
    let claim = claimed(
        store
            .claim_schema_verification(claim_command(
                &format!("{key}-verify"),
                digest,
                logical_now,
                logical_now + 10,
            ))
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
                id: format!("{key}-receipt"),
                verifier_version: "v1".into(),
                input_digest: format!("sha256:{}", "c".repeat(64)),
                passed: true,
            },
        )
        .await
        .unwrap()
        .fence
}

#[tokio::test]
async fn active_pointer_change_preserves_existing_pin_writes_but_fences_new_old_pins() {
    let store = SimEventStore::no_faults(8);
    let original = format!("sha256:{}", "a".repeat(64));
    let replacement = format!("sha256:{}", "b".repeat(64));
    store
        .submit_schema_bundle(command(
            "original-submit",
            &format!("sha256:{}", "1".repeat(64)),
            &original,
        ))
        .await
        .unwrap();
    let original_fence = verify_bundle(&store, "original", &original, 1).await;
    activated(
        store
            .activate_schema_bundle(activation_command(
                "original-activate",
                &original,
                None,
                original_fence,
                "original-receipt",
            ))
            .await
            .unwrap(),
    );
    let existing_id = scoped_persistence_id("Task", "existing", &original);
    store
        .append(&existing_id, 0, &[test_event(1)])
        .await
        .unwrap();
    let inactive_scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-same-digest-inactive".into(),
    };
    let cross_scope_id =
        scoped_persistence_id_in_scope("Task", "cross-scope", &inactive_scope, &original);
    assert!(
        store
            .append(&cross_scope_id, 0, &[test_event(1)])
            .await
            .unwrap_err()
            .to_string()
            .contains("stale scoped schema write fence")
    );

    store
        .submit_schema_bundle(command_with_predecessor(
            "replacement-submit",
            &format!("sha256:{}", "2".repeat(64)),
            &replacement,
            Some(&original),
        ))
        .await
        .unwrap();
    let replacement_fence = verify_bundle(&store, "replacement", &replacement, 2).await;
    activated(
        store
            .activate_schema_bundle(activation_command(
                "replacement-activate",
                &replacement,
                Some(&original),
                replacement_fence,
                "replacement-receipt",
            ))
            .await
            .unwrap(),
    );

    assert_eq!(
        store
            .append(&existing_id, 1, &[test_event(2)])
            .await
            .unwrap(),
        2
    );
    let stale_new_id = scoped_persistence_id("Task", "new-old", &original);
    assert!(
        store
            .append(&stale_new_id, 0, &[test_event(1)])
            .await
            .unwrap_err()
            .to_string()
            .contains("stale scoped schema write fence")
    );
    let active_new_id = scoped_persistence_id("Task", "new-active", &replacement);
    assert_eq!(
        store
            .append(&active_new_id, 0, &[test_event(1)])
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn submit_is_atomic_idempotent_and_conflict_detecting() {
    let store = SimEventStore::no_faults(7);
    let digest = format!("sha256:{}", "a".repeat(64));
    let request = format!("sha256:{}", "b".repeat(64));
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

    store.fail_next_schema_operations(SimSchemaFaultPoint::SubmitBundle, 1);
    let failure = store
        .submit_schema_bundle(command("submit-1", &request, &digest))
        .await
        .unwrap_err();
    assert_injected_failure(failure, SimSchemaFaultPoint::SubmitBundle);
    assert!(
        store
            .get_schema_deployment("tenant-a", &scope(), &digest)
            .await
            .unwrap()
            .is_none()
    );
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
    let conflict = store
        .submit_schema_bundle(command(
            "submit-1",
            &format!("sha256:{}", "c".repeat(64)),
            &digest,
        ))
        .await
        .unwrap_err();
    assert_eq!(conflict, SchemaDeploymentStoreError::IdempotencyConflict);

    let stored = store
        .get_schema_deployment("tenant-a", &scope(), &digest)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, SchemaDeploymentStatus::Submitted);
    assert_eq!(stored.committed_sequence, 1);
}

#[tokio::test]
async fn expired_claim_advances_fence_and_rejects_stale_finisher() {
    let store = SimEventStore::no_faults(8);
    let digest = format!("sha256:{}", "d".repeat(64));
    let request = format!("sha256:{}", "e".repeat(64));
    store
        .submit_schema_bundle(command("submit-2", &request, &digest))
        .await
        .unwrap();

    store.fail_next_schema_operations(SimSchemaFaultPoint::ClaimVerification, 1);
    let failure = store
        .claim_schema_verification(claim_command("verify-1", &digest, 10, 20))
        .await
        .unwrap_err();
    assert_injected_failure(failure, SimSchemaFaultPoint::ClaimVerification);
    let unclaimed = store
        .get_schema_deployment("tenant-a", &scope(), &digest)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unclaimed.status, SchemaDeploymentStatus::Submitted);
    assert_eq!(unclaimed.fence, 0);
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

    let stale = store
        .finish_schema_verification(
            "tenant-a",
            &scope(),
            &digest,
            first.fence,
            SchemaVerificationReceipt {
                id: "verify-1".into(),
                verifier_version: "v1".into(),
                input_digest: request,
                passed: true,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(stale, SchemaDeploymentStoreError::StaleFence);
}

#[tokio::test]
async fn activation_compares_receipt_predecessor_and_fence_atomically() {
    let store = SimEventStore::no_faults(9);
    let digest = format!("sha256:{}", "f".repeat(64));
    let request = format!("sha256:{}", "1".repeat(64));
    store
        .submit_schema_bundle(command("submit-3", &request, &digest))
        .await
        .unwrap();
    let claim = claimed(
        store
            .claim_schema_verification(claim_command("verify-3", &digest, 1, 10))
            .await
            .unwrap(),
    );
    let receipt = SchemaVerificationReceipt {
        id: "verify-3".into(),
        verifier_version: "v1".into(),
        input_digest: request,
        passed: true,
    };
    store.fail_next_schema_operations(SimSchemaFaultPoint::FinishVerification, 1);
    let failure = store
        .finish_schema_verification("tenant-a", &scope(), &digest, claim.fence, receipt.clone())
        .await
        .unwrap_err();
    assert_injected_failure(failure, SimSchemaFaultPoint::FinishVerification);
    assert_eq!(
        store
            .get_schema_deployment("tenant-a", &scope(), &digest)
            .await
            .unwrap()
            .unwrap()
            .status,
        SchemaDeploymentStatus::Verifying
    );
    let verified = store
        .finish_schema_verification("tenant-a", &scope(), &digest, claim.fence, receipt)
        .await
        .unwrap();
    assert!(matches!(
        store
            .claim_schema_verification(claim_command("verify-3", &digest, 2, 11))
            .await
            .unwrap(),
        ClaimSchemaVerificationOutcome::Replayed(record)
            if record.status == SchemaDeploymentStatus::Verified
    ));
    let mut conflicting_claim = claim_command("verify-3", &digest, 2, 11);
    conflicting_claim.operation.request_digest = format!("sha256:{}", "e".repeat(64));
    assert_eq!(
        store
            .claim_schema_verification(conflicting_claim)
            .await
            .unwrap_err(),
        SchemaDeploymentStoreError::IdempotencyConflict
    );

    let missing_digest = format!("sha256:{}", "f".repeat(64));
    let stale = store
        .activate_schema_bundle(activation_command(
            "activate-stale",
            &digest,
            Some(&missing_digest),
            verified.fence,
            "verify-3",
        ))
        .await
        .unwrap_err();
    assert_eq!(stale, SchemaDeploymentStoreError::PredecessorMismatch);
    assert!(
        store
            .active_schema_pointer("tenant-a", &scope())
            .await
            .unwrap()
            .is_none()
    );

    store.fail_next_schema_operations(SimSchemaFaultPoint::ActivateBundle, 1);
    let failure = store
        .activate_schema_bundle(activation_command(
            "activate-3",
            &digest,
            None,
            verified.fence,
            "verify-3",
        ))
        .await
        .unwrap_err();
    assert_injected_failure(failure, SimSchemaFaultPoint::ActivateBundle);
    assert!(
        store
            .active_schema_pointer("tenant-a", &scope())
            .await
            .unwrap()
            .is_none()
    );
    let activation = activation_command("activate-3", &digest, None, verified.fence, "verify-3");
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
        activation_command("activate-3", &digest, None, verified.fence, "verify-3");
    conflicting_activation.operation.request_digest = format!("sha256:{}", "e".repeat(64));
    assert_eq!(
        store
            .activate_schema_bundle(conflicting_activation)
            .await
            .unwrap_err(),
        SchemaDeploymentStoreError::IdempotencyConflict
    );
    assert_eq!(pointer.bundle_digest, digest);
    assert_eq!(
        store
            .active_schema_pointer("tenant-a", &scope())
            .await
            .unwrap(),
        Some(pointer.clone())
    );
    let pinned_id = scoped_persistence_id("Task", "entity-1", &digest);
    store
        .append(
            &pinned_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Configure".into(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: "pin-contract".into(),
                },
            }],
        )
        .await
        .unwrap();
    let collision_base = "entity-collision";
    let collision_entity = temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
        collision_base,
        &temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
            scope: scope(),
            bundle_digest: digest.clone(),
        },
    );
    store
        .append(
            &scoped_persistence_id("Task", &collision_entity, &digest),
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Configure".into(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: "pin-collision-contract".into(),
                },
            }],
        )
        .await
        .unwrap();
    assert!(
        store
            .scoped_entity_bundle_digests("tenant-a", "Task", collision_base, &scope(), 2)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .scoped_entity_bundle_digests("tenant-a", "Task", &collision_entity, &scope(), 2)
            .await
            .unwrap(),
        vec![digest.clone()]
    );
    assert_eq!(
        store
            .scoped_entity_bundle_digests("tenant-a", "Task", "entity-1", &scope(), 2)
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
    store.fail_next_schema_operations(SimSchemaFaultPoint::RetireBundle, 1);
    let failure = store
        .retire_schema_bundle(retire_command("retire-3", &digest, pointer.fence))
        .await
        .unwrap_err();
    assert_injected_failure(failure, SimSchemaFaultPoint::RetireBundle);
    assert_eq!(
        store
            .active_schema_pointer("tenant-a", &scope())
            .await
            .unwrap(),
        Some(pointer.clone())
    );
    let retirement = retire_command("retire-3", &digest, pointer.fence);
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
    let mut conflicting_retirement = retire_command("retire-3", &digest, pointer.fence);
    conflicting_retirement.operation.request_digest = format!("sha256:{}", "e".repeat(64));
    assert_eq!(
        store
            .retire_schema_bundle(conflicting_retirement)
            .await
            .unwrap_err(),
        SchemaDeploymentStoreError::IdempotencyConflict
    );
    assert_eq!(retired.status, SchemaDeploymentStatus::Retired);
    assert!(retired.fence > pointer.fence);
    assert!(
        store
            .active_schema_pointer("tenant-a", &scope())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn randomized_multi_tenant_activation_interleavings_preserve_one_scope_winner() {
    for seed in 0..16u64 {
        let store = SimEventStore::no_faults(seed);
        let tenant = format!("tenant-{seed}");
        let test_scope = SchemaScope {
            kind: SchemaScopeKind::Task,
            id: format!("task-{seed}"),
        };
        let mut verified = Vec::new();
        for candidate in 0..2u8 {
            let digest = format!("sha256:{seed:016x}{candidate:048x}");
            let key = format!("submit-{seed}-{candidate}");
            let mut submit = command(
                &key,
                &format!("sha256:{seed:016x}{candidate:048x}"),
                &digest,
            );
            submit.bundle.tenant = tenant.clone();
            submit.bundle.scope = test_scope.clone();
            store.submit_schema_bundle(submit).await.unwrap();
            let receipt_id = format!("verify-{seed}-{candidate}");
            let mut claim = claim_command(&receipt_id, &digest, 1, 10);
            claim.tenant = tenant.clone();
            claim.scope = test_scope.clone();
            let claim = claimed(store.claim_schema_verification(claim).await.unwrap());
            let record = store
                .finish_schema_verification(
                    &tenant,
                    &test_scope,
                    &digest,
                    claim.fence,
                    SchemaVerificationReceipt {
                        id: receipt_id.clone(),
                        verifier_version: "dst/v1".into(),
                        input_digest: format!("sha256:{}", "e".repeat(64)),
                        passed: true,
                    },
                )
                .await
                .unwrap();
            verified.push((digest, receipt_id, record.fence));
        }
        let commands = verified
            .into_iter()
            .enumerate()
            .map(
                |(candidate, (digest, receipt_id, fence))| ActivateSchemaBundle {
                    tenant: tenant.clone(),
                    scope: test_scope.clone(),
                    bundle_digest: digest,
                    expected_predecessor: None,
                    expected_fence: fence,
                    verification_receipt_id: receipt_id,
                    operation: operation(&format!("activate-{seed}-{candidate}")),
                },
            )
            .collect::<Vec<_>>();
        let left_store = store.clone();
        let right_store = store.clone();
        let left_command = commands[0].clone();
        let right_command = commands[1].clone();
        let left = tokio::spawn(async move {
            for _ in 0..(seed % 3) {
                tokio::task::yield_now().await;
            }
            left_store.activate_schema_bundle(left_command).await
        });
        let right = tokio::spawn(async move {
            for _ in 0..((seed + 1) % 3) {
                tokio::task::yield_now().await;
            }
            right_store.activate_schema_bundle(right_command).await
        });
        let results = [left.await.unwrap(), right.await.unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(
                    result,
                    Err(SchemaDeploymentStoreError::PredecessorMismatch)
                ))
                .count(),
            1
        );
        assert!(
            store
                .active_schema_pointer(&tenant, &test_scope)
                .await
                .unwrap()
                .is_some()
        );
    }
}

#[path = "schema_deployment/migration.rs"]
mod migration;
