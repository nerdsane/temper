#[cfg(test)]
const _: () = ();

use super::*;
use temper_wasm_sdk::schema_deployment::GetSchemaMigrationRequestV1;

#[tokio::test]
async fn governed_migration_materializes_shadow_and_atomically_cuts_over() {
    const UNCHANGED_MODULE: &[u8] = br#"
        (module
          (memory (export "memory") 1)
          (data (i32.const 4096) "{\22outcome\22:\22unchanged\22}")
          (func (export "temper_schema_alloc_v1") (param i32) (result i32) i32.const 1024)
          (func (export "temper_schema_dealloc_v1") (param i32 i32))
          (func (export "temper_schema_migrate_v1") (param i32 i32) (result i64)
            i64.const 17592186044439))
    "#;
    let invocation = schema_migration_invocation();
    let state = &invocation.state;
    let security = SecurityContext::system();
    let service = crate::schema_deployment::GovernedSchemaDeploymentService::new(state);
    let source_receipt = service
        .submit("default", &security, request())
        .await
        .unwrap();
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-42".into(),
    };
    let store = state
        .storage_stack
        .as_ref()
        .unwrap()
        .schema_deployments
        .as_ref()
        .unwrap();
    let source_claim = match store
        .claim_schema_verification(ClaimSchemaVerification {
            tenant: "default".into(),
            scope: scope.clone(),
            bundle_digest: source_receipt.bundle_digest.clone(),
            logical_now: 1,
            lease_expires_at: 10,
            operation: SchemaOperationIdentity {
                idempotency_key: "source-verify".into(),
                request_digest: format!("sha256:{}", "3".repeat(64)),
                request_id: "source-verify-request".into(),
            },
        })
        .await
        .unwrap()
    {
        ClaimSchemaVerificationOutcome::Claimed(record)
        | ClaimSchemaVerificationOutcome::Replayed(record) => record,
    };
    let source_verified = store
        .finish_schema_verification(
            "default",
            &scope,
            &source_receipt.bundle_digest,
            source_claim.fence,
            SchemaVerificationReceipt {
                id: "source-verification".into(),
                verifier_version: "test/v1".into(),
                input_digest: format!("sha256:{}", "1".repeat(64)),
                passed: true,
            },
        )
        .await
        .unwrap();
    let source_active = service
        .activate(
            "default",
            &security,
            ActivateSchemaBundleRequestV1 {
                request_id: "activate-source".into(),
                idempotency_key: "activate-source".into(),
                scope: SchemaScopeV1 {
                    kind: "task".into(),
                    id: "task-42".into(),
                },
                bundle_digest: source_receipt.bundle_digest.clone(),
                expected_predecessor: None,
                expected_fence: source_verified.fence,
                verification_receipt_id: "source-verification".into(),
                stream_descriptor_completion_receipt_id: None,
            },
        )
        .await
        .unwrap();
    let source_active_replay = service
        .activate(
            "default",
            &security,
            ActivateSchemaBundleRequestV1 {
                request_id: "activate-source-replay".into(),
                idempotency_key: "activate-source".into(),
                scope: SchemaScopeV1 {
                    kind: "task".into(),
                    id: "task-42".into(),
                },
                bundle_digest: source_receipt.bundle_digest.clone(),
                expected_predecessor: None,
                expected_fence: source_verified.fence,
                verification_receipt_id: "source-verification".into(),
                stream_descriptor_completion_receipt_id: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(source_active_replay.fence, source_active.fence);
    assert_eq!(
        source_active_replay.committed_sequence,
        source_active.committed_sequence
    );
    for entity_id in ["task-1", "task-2"] {
        state
            .get_or_create_scoped_entity(
                &TenantId::default(),
                "Task",
                entity_id,
                serde_json::json!({"Id": entity_id}),
                SchemaExecutionPin {
                    scope: scope.clone(),
                    bundle_digest: source_receipt.bundle_digest.clone(),
                },
            )
            .await
            .unwrap();
    }

    let engine_hash = state
        .wasm_engine
        .compile_and_cache(UNCHANGED_MODULE)
        .unwrap();
    let module_digest = format!("sha256:{engine_hash}");
    let budgets = ScopedBundleBudgets::default();
    let compiled = ScopedSpecBundle::compile(ScopedSpecBundleInput {
        scope_id: "task-42".into(),
        predecessor_digest: Some(source_receipt.bundle_digest.clone()),
        csdl_xml: CSDL.into(),
        ioa_sources: vec![IoaSourceInput {
            entity_type: "Example.Task".into(),
            source: IOA.into(),
        }],
        cedar_policies: vec![],
        wasm_modules: vec![],
        migration: Some(MigrationArtifactInput {
            name: "reshape".into(),
            artifact_digest: module_digest.clone(),
            abi_version: "temper-schema-migration/v1".into(),
        }),
        budgets: budgets.clone(),
    })
    .unwrap();
    let target_request = SubmitSchemaBundleRequestV1 {
        request_id: "target-submit".into(),
        idempotency_key: "target-submit".into(),
        scope: SchemaScopeV1 {
            kind: "task".into(),
            id: "task-42".into(),
        },
        expected_predecessor: Some(source_receipt.bundle_digest.clone()),
        expected_digest: compiled.digest().into(),
        canonicalization_version: temper_spec::bundle::SCOPED_SPEC_BUNDLE_CONTRACT_V1.into(),
        csdl: CSDL.into(),
        ioa: vec![SchemaIoaSourceV1 {
            entity_type: "Example.Task".into(),
            source: IOA.into(),
        }],
        cedar_policies: vec![],
        wasm_modules: vec![],
        migration: Some(SchemaMigrationArtifactV1 {
            name: "reshape".into(),
            artifact_digest: module_digest,
            abi_version: "temper-schema-migration/v1".into(),
        }),
        budgets: SchemaBundleBudgetsV1 {
            verification_steps: budgets.verification_steps,
            migration_fuel_per_entity: budgets.migration_fuel_per_entity,
            migration_memory_pages: budgets.migration_memory_pages,
            migration_input_bytes: budgets.migration_input_bytes,
            migration_output_bytes: budgets.migration_output_bytes,
            migration_entities_per_batch: budgets.migration_entities_per_batch,
            migration_total_entities: budgets.migration_total_entities,
            migration_total_batches: budgets.migration_total_batches,
            migration_attempts: budgets.migration_attempts,
        },
    };
    let target_receipt = service
        .submit("default", &security, target_request.clone())
        .await
        .unwrap();
    let target_claim = match store
        .claim_schema_verification(ClaimSchemaVerification {
            tenant: "default".into(),
            scope: scope.clone(),
            bundle_digest: target_receipt.bundle_digest.clone(),
            logical_now: 1,
            lease_expires_at: 10,
            operation: SchemaOperationIdentity {
                idempotency_key: "target-verify".into(),
                request_digest: format!("sha256:{}", "4".repeat(64)),
                request_id: "target-verify-request".into(),
            },
        })
        .await
        .unwrap()
    {
        ClaimSchemaVerificationOutcome::Claimed(record)
        | ClaimSchemaVerificationOutcome::Replayed(record) => record,
    };
    store
        .finish_schema_verification(
            "default",
            &scope,
            &target_receipt.bundle_digest,
            target_claim.fence,
            SchemaVerificationReceipt {
                id: "target-verification".into(),
                verifier_version: "test/v1".into(),
                input_digest: format!("sha256:{}", "2".repeat(64)),
                passed: true,
            },
        )
        .await
        .unwrap();
    const REJECT_MODULE: &[u8] = br#"
        (module
          (memory (export "memory") 1)
          (data (i32.const 4096) "{\22outcome\22:\22reject\22,\22code\22:\22nope\22,\22message\22:\22bad\22}")
          (func (export "temper_schema_alloc_v1") (param i32) (result i32) i32.const 1024)
          (func (export "temper_schema_dealloc_v1") (param i32 i32))
          (func (export "temper_schema_migrate_v1") (param i32 i32) (result i64)
            i64.const 17592186044466))
    "#;
    let reject_hash = state.wasm_engine.compile_and_cache(REJECT_MODULE).unwrap();
    let reject_digest = format!("sha256:{reject_hash}");
    let reject_compiled = ScopedSpecBundle::compile(ScopedSpecBundleInput {
        scope_id: "task-42".into(),
        predecessor_digest: Some(source_receipt.bundle_digest.clone()),
        csdl_xml: CSDL.into(),
        ioa_sources: vec![IoaSourceInput {
            entity_type: "Example.Task".into(),
            source: IOA.into(),
        }],
        cedar_policies: vec![],
        wasm_modules: vec![],
        migration: Some(MigrationArtifactInput {
            name: "reject".into(),
            artifact_digest: reject_digest.clone(),
            abi_version: "temper-schema-migration/v1".into(),
        }),
        budgets: budgets.clone(),
    })
    .unwrap();
    let mut reject_request = target_request.clone();
    reject_request.request_id = "reject-target-submit".into();
    reject_request.idempotency_key = "reject-target-submit".into();
    reject_request.expected_digest = reject_compiled.digest().into();
    reject_request.migration = Some(SchemaMigrationArtifactV1 {
        name: "reject".into(),
        artifact_digest: reject_digest,
        abi_version: "temper-schema-migration/v1".into(),
    });
    let reject_target = service
        .submit("default", &security, reject_request)
        .await
        .unwrap();
    let reject_claim = match store
        .claim_schema_verification(ClaimSchemaVerification {
            tenant: "default".into(),
            scope: scope.clone(),
            bundle_digest: reject_target.bundle_digest.clone(),
            logical_now: 1,
            lease_expires_at: 10,
            operation: SchemaOperationIdentity {
                idempotency_key: "reject-target-verify".into(),
                request_digest: format!("sha256:{}", "5".repeat(64)),
                request_id: "reject-target-verify-request".into(),
            },
        })
        .await
        .unwrap()
    {
        ClaimSchemaVerificationOutcome::Claimed(record)
        | ClaimSchemaVerificationOutcome::Replayed(record) => record,
    };
    store
        .finish_schema_verification(
            "default",
            &scope,
            &reject_target.bundle_digest,
            reject_claim.fence,
            SchemaVerificationReceipt {
                id: "reject-target-verification".into(),
                verifier_version: "test/v1".into(),
                input_digest: format!("sha256:{}", "6".repeat(64)),
                passed: true,
            },
        )
        .await
        .unwrap();
    let reject_start = StartSchemaMigrationRequestV1 {
        request_id: "reject-migration-start".into(),
        idempotency_key: "reject-migration-start".into(),
        scope: SchemaScopeV1 {
            kind: "task".into(),
            id: "task-42".into(),
        },
        source_bundle_digest: source_receipt.bundle_digest.clone(),
        target_bundle_digest: reject_target.bundle_digest,
        verification_receipt_id: "reject-target-verification".into(),
        expected_fence: source_active.fence,
        budgets: SchemaMigrationBudgetsV1 {
            fuel_per_entity: 100_000,
            memory_pages: 2,
            input_bytes: 8_192,
            output_bytes: 8_192,
            entities_per_batch: 1,
            total_entities: 100,
            total_batches: 10,
            attempts: 3,
        },
    };
    let rejected = service
        .start_migration("default", &security, reject_start.clone())
        .await
        .unwrap();
    assert_eq!(rejected.status, "rejected");
    assert!(rejected.validation_receipt_id.is_some());
    assert!(rejected.migration_receipt_id.is_some());
    let rejected_replay = service
        .start_migration("default", &security, reject_start)
        .await
        .unwrap();
    assert_eq!(rejected_replay, rejected);
    let migrated = service
        .start_migration(
            "default",
            &security,
            StartSchemaMigrationRequestV1 {
                request_id: "migration-start".into(),
                idempotency_key: "migration-start".into(),
                scope: SchemaScopeV1 {
                    kind: "task".into(),
                    id: "task-42".into(),
                },
                source_bundle_digest: source_receipt.bundle_digest.clone(),
                target_bundle_digest: target_receipt.bundle_digest.clone(),
                verification_receipt_id: "target-verification".into(),
                expected_fence: source_active.fence,
                budgets: SchemaMigrationBudgetsV1 {
                    fuel_per_entity: 100_000,
                    memory_pages: 2,
                    input_bytes: 8_192,
                    output_bytes: 8_192,
                    entities_per_batch: 1,
                    total_entities: 100,
                    total_batches: 10,
                    attempts: 3,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(migrated.status, "migrating", "{migrated:?}");
    state
        .update_scoped_entity_fields_if_sequence(
            &TenantId::default(),
            "Task",
            "task-1",
            serde_json::json!({"Title": "changed-during-migration"}),
            false,
            None,
            SchemaExecutionPin {
                scope: scope.clone(),
                bundle_digest: source_receipt.bundle_digest.clone(),
            },
        )
        .await
        .unwrap();
    let mut migrated = migrated;
    for poll in 0..50 {
        if migrated.status == "completed" {
            break;
        }
        tokio::task::yield_now().await;
        migrated = service
            .get_migration(
                "default",
                &security,
                GetSchemaMigrationRequestV1 {
                    request_id: format!("migration-poll-{poll}"),
                    scope: SchemaScopeV1 {
                        kind: "task".into(),
                        id: "task-42".into(),
                    },
                    job_id: migrated.job_id.clone(),
                },
            )
            .await
            .unwrap();
    }
    assert_eq!(migrated.status, "completed");
    assert_eq!(migrated.consumed_entities, 3);
    let retry_key = "migration-retry-after-completion";
    let retry = service
        .retry_migration(
            "default",
            &security,
            RetrySchemaMigrationRequestV1 {
                request_id: "migration-retry-original".into(),
                idempotency_key: retry_key.into(),
                scope: SchemaScopeV1 {
                    kind: "task".into(),
                    id: "task-42".into(),
                },
                job_id: migrated.job_id.clone(),
            },
        )
        .await
        .unwrap();
    let replay = service
        .retry_migration(
            "default",
            &security,
            RetrySchemaMigrationRequestV1 {
                request_id: "migration-retry-replay".into(),
                idempotency_key: retry_key.into(),
                scope: SchemaScopeV1 {
                    kind: "task".into(),
                    id: "task-42".into(),
                },
                job_id: migrated.job_id.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(retry.committed_sequence, migrated.committed_sequence);
    assert_eq!(replay.request_id, retry.request_id);
    assert_eq!(
        state
            .registry
            .read()
            .unwrap()
            .active_scope_digest(&TenantId::default(), &scope),
        Some(target_receipt.bundle_digest.as_str())
    );
    let target_state = state
        .get_scoped_entity_state(
            &TenantId::default(),
            "Task",
            "task-1",
            SchemaExecutionPin {
                scope,
                bundle_digest: target_receipt.bundle_digest,
            },
        )
        .await
        .unwrap();
    assert_eq!(target_state.state.fields["Id"], "task-1");
    assert_eq!(
        target_state.state.fields["title"],
        "changed-during-migration"
    );

    #[derive(serde::Deserialize)]
    struct TypedSourceState {
        id: String,
        title: String,
    }
    let shadow_rows = store
        .page_schema_migration_shadow("default", &migrated.job_id, None, 10)
        .await
        .unwrap();
    let task_1 = shadow_rows
        .iter()
        .find(|row| row.entity_id == "task-1")
        .expect("task-1 migration output");
    let typed: TypedSourceState =
        temper_wasm_sdk::decode_source_state(&task_1.canonical_state_json)
            .expect("real migration output should retain typed snake_case source state");
    assert_eq!(typed.id, "task-1");
    assert_eq!(typed.title, "changed-during-migration");
}
