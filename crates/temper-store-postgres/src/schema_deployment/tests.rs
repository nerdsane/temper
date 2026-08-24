use std::collections::BTreeMap;

use sqlx::PgPool;
use temper_runtime::persistence::schema_deployment::{
    ActivateSchemaBundle, ActivateSchemaBundleOutcome, ClaimSchemaVerification,
    ClaimSchemaVerificationOutcome, CommitSchemaMigrationBatch, CreateSchemaMigration,
    ReserveSchemaMigrationRetry, SchemaActivePointer, SchemaBundleRecord, SchemaDeploymentRecord,
    SchemaDeploymentStore, SchemaDeploymentStoreError, SchemaExecutionPin,
    SchemaMigrationBatchReceipt, SchemaMigrationBudgets, SchemaMigrationShadowRow,
    SchemaMigrationStatus, SchemaMigrationValidationReceipt, SchemaOperationIdentity, SchemaScope,
    SchemaScopeKind, SchemaVerificationReceipt, SubmitSchemaBundle,
};
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};

use crate::{PostgresEventStore, migration::run_migrations};

mod migration_fence_race;

fn scope(id: &str) -> SchemaScope {
    SchemaScope {
        kind: SchemaScopeKind::Task,
        id: id.to_string(),
    }
}

fn submission(
    tenant: &str,
    scope: &SchemaScope,
    key: &str,
    request_digest: &str,
    bundle_digest: &str,
) -> SubmitSchemaBundle {
    SubmitSchemaBundle {
        bundle: SchemaBundleRecord {
            tenant: tenant.to_string(),
            scope: scope.clone(),
            digest: bundle_digest.to_string(),
            predecessor_digest: None,
            canonical_csdl: "<canonical/>".to_string(),
            canonical_ioa: BTreeMap::from([(
                "Example.Task".to_string(),
                "[automaton]".to_string(),
            )]),
            cedar_policies: BTreeMap::new(),
            wasm_module_digests: BTreeMap::new(),
            migration_module_name: None,
            migration_module_digest: None,
            migration_abi_version: None,
            canonical_budgets: "verification_steps=100".to_string(),
        },
        idempotency_key: key.to_string(),
        request_digest: request_digest.to_string(),
        request_id: format!("request-{key}"),
    }
}

fn operation(key: &str) -> SchemaOperationIdentity {
    SchemaOperationIdentity {
        idempotency_key: key.to_string(),
        request_digest: format!("sha256:{}", "d".repeat(64)),
        request_id: format!("request-{key}"),
    }
}

fn claimed(outcome: ClaimSchemaVerificationOutcome) -> SchemaDeploymentRecord {
    match outcome {
        ClaimSchemaVerificationOutcome::Claimed(record)
        | ClaimSchemaVerificationOutcome::Replayed(record) => record,
    }
}

fn activated(outcome: ActivateSchemaBundleOutcome) -> SchemaActivePointer {
    match outcome {
        ActivateSchemaBundleOutcome::Activated(pointer)
        | ActivateSchemaBundleOutcome::Replayed(pointer) => pointer,
    }
}

async fn verify(
    store: &PostgresEventStore,
    tenant: &str,
    scope: &SchemaScope,
    bundle_digest: &str,
    input_digest: &str,
    receipt_id: &str,
) -> temper_runtime::persistence::schema_deployment::SchemaDeploymentRecord {
    let claim = claimed(
        store
            .claim_schema_verification(ClaimSchemaVerification {
                tenant: tenant.to_string(),
                scope: scope.clone(),
                bundle_digest: bundle_digest.to_string(),
                logical_now: 1,
                lease_expires_at: 10,
                operation: operation(receipt_id),
            })
            .await
            .expect("claim verification"),
    );
    store
        .finish_schema_verification(
            tenant,
            scope,
            bundle_digest,
            claim.fence,
            SchemaVerificationReceipt {
                id: receipt_id.to_string(),
                verifier_version: "test/v1".to_string(),
                input_digest: input_digest.to_string(),
                passed: true,
            },
        )
        .await
        .expect("finish verification")
}

#[test]
fn postgres_schema_migration_contract() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("skipping Postgres schema migration test: DATABASE_URL is not set");
            return;
        }
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url)
            .await
            .expect("connect to DATABASE_URL");
        run_migrations(&pool).await.expect("run migrations");
        let store = PostgresEventStore::new(pool);
        let suffix = uuid::Uuid::new_v4();
        let tenant = format!("schema-migration-{suffix}");
        let scope = scope(&format!("task-{suffix}"));
        let source_digest = format!("sha256:{}", "1".repeat(64));
        let source_request = format!("sha256:{}", "2".repeat(64));
        store
            .submit_schema_bundle(submission(
                &tenant,
                &scope,
                "source-submit",
                &source_request,
                &source_digest,
            ))
            .await
            .expect("submit source");
        let source = verify(
            &store,
            &tenant,
            &scope,
            &source_digest,
            &source_request,
            "source-verify",
        )
        .await;
        let source_pointer = activated(
            store
                .activate_schema_bundle(ActivateSchemaBundle {
                    tenant: tenant.clone(),
                    scope: scope.clone(),
                    bundle_digest: source_digest.clone(),
                    expected_predecessor: None,
                    expected_fence: source.fence,
                    verification_receipt_id: "source-verify".to_string(),
                    operation: operation("source-activate"),
                })
                .await
                .expect("activate source"),
        );
        let source_pin = SchemaExecutionPin {
            scope: scope.clone(),
            bundle_digest: source_digest.clone(),
        };
        let inactive_pin = SchemaExecutionPin {
            scope: SchemaScope {
                kind: SchemaScopeKind::Task,
                id: format!("inactive-{suffix}"),
            },
            bundle_digest: source_digest.clone(),
        };
        let inactive_id = format!(
            "{tenant}:Example.Task:{}",
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
                        },
                    }],
                )
                .await
                .expect_err("same digest in an inactive scope must remain fenced")
                .to_string()
                .contains("stale scoped schema write fence")
        );
        let pinned_id = format!(
            "{tenant}:Example.Task:{}",
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                "entity-雪",
                &source_pin,
            )
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
                    },
                }],
            )
            .await
            .expect("append pinned entity event");
        let collision_base = "entity-collision";
        let collision_entity =
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                collision_base,
                &source_pin,
            );
        store
            .append(
                &format!(
                    "{tenant}:Example.Task:{}",
                    temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                        &collision_entity,
                        &source_pin,
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
                    },
                }],
            )
            .await
            .expect("append colon-bearing pinned entity event");
        assert!(
            store
                .scoped_entity_bundle_digests(&tenant, "Example.Task", collision_base, &scope, 2,)
                .await
                .expect("reject colliding entity prefix")
                .is_empty()
        );
        assert_eq!(
            store
                .scoped_entity_bundle_digests(
                    &tenant,
                    "Example.Task",
                    &collision_entity,
                    &scope,
                    2,
                )
                .await
                .expect("load colon-bearing entity pin"),
            vec![source_digest.clone()]
        );
        assert_eq!(
            store
                .scoped_entity_bundle_digests(&tenant, "Example.Task", "entity-雪", &scope, 2,)
                .await
                .expect("load pinned entity digest"),
            vec![source_digest.clone()]
        );

        let target_digest = format!("sha256:{}", "3".repeat(64));
        let target_request = format!("sha256:{}", "4".repeat(64));
        let module_digest = format!("sha256:{}", "5".repeat(64));
        let mut target = submission(
            &tenant,
            &scope,
            "target-submit",
            &target_request,
            &target_digest,
        );
        target.bundle.predecessor_digest = Some(source_digest.clone());
        target.bundle.migration_module_name = Some("reshape".to_string());
        target.bundle.migration_module_digest = Some(module_digest.clone());
        target.bundle.migration_abi_version = Some("temper-schema-migration/v1".to_string());
        store
            .submit_schema_bundle(target)
            .await
            .expect("submit target");
        verify(
            &store,
            &tenant,
            &scope,
            &target_digest,
            &target_request,
            "target-verify",
        )
        .await;

        let job_id = format!("migration-{suffix}");
        store
            .create_schema_migration(CreateSchemaMigration {
                job_id: job_id.clone(),
                tenant: tenant.clone(),
                scope: scope.clone(),
                source_bundle_digest: source_digest.clone(),
                target_bundle_digest: target_digest.clone(),
                verification_receipt_id: "target-verify".to_string(),
                source_expected_fence: source_pointer.fence,
                module_name: "reshape".to_string(),
                module_digest,
                accepted_authority_json: r#"{"principal":"agent-a"}"#.to_string(),
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
                idempotency_key: "migration-start".to_string(),
                request_digest: format!("sha256:{}", "6".repeat(64)),
                request_id: "migration-request".to_string(),
            })
            .await
            .expect("create migration");
        let retry = ReserveSchemaMigrationRetry {
            tenant: tenant.clone(),
            job_id: job_id.clone(),
            operation: operation("migration-retry-1"),
        };
        let reserved = store
            .reserve_schema_migration_retry(retry.clone())
            .await
            .expect("reserve migration retry");
        assert!(!reserved.replayed);
        assert_eq!(reserved.starting_sequence, 1);
        assert!(
            store
                .reserve_schema_migration_retry(retry)
                .await
                .expect("replay migration retry")
                .replayed
        );
        let claim = store
            .claim_schema_migration(&tenant, &job_id, 10, 20)
            .await
            .expect("claim migration");
        let row = SchemaMigrationShadowRow {
            entity_type: "Example.Task".to_string(),
            entity_id: "task-1".to_string(),
            source_sequence: 4,
            canonical_state_json: r#"{"Id":"task-1","Status":"Open"}"#.to_string(),
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
                },
            },
        };
        let cursor = Some((row.entity_type.clone(), row.entity_id.clone()));
        let batch = CommitSchemaMigrationBatch {
            job_id: job_id.clone(),
            expected_fence: claim.fence,
            expected_cursor: None,
            next_cursor: cursor.clone(),
            scan_complete: true,
            restart_scan: false,
            observed_source_write_version: 0,
            rows: vec![row.clone()],
            receipt: SchemaMigrationBatchReceipt {
                id: "batch-1".to_string(),
                source_cursor: None,
                next_cursor: cursor,
                input_digest: row.input_digest.clone(),
                output_digest: row.output_digest.clone(),
                row_count: 1,
            },
        };
        let validating = store
            .commit_schema_migration_batch(&tenant, batch.clone())
            .await
            .expect("commit migration batch");
        assert_eq!(validating.status, SchemaMigrationStatus::Validating);
        assert_eq!(
            store
                .commit_schema_migration_batch(&tenant, batch)
                .await
                .expect("replay migration batch"),
            validating
        );
        let ready = store
            .validate_schema_migration(
                &tenant,
                &job_id,
                validating.fence,
                SchemaMigrationValidationReceipt {
                    id: "validation-1".to_string(),
                    shadow_digest: row.output_digest.clone(),
                    caught_up_sequence: 0,
                    passed: true,
                },
            )
            .await
            .expect("validate migration");
        let pointer = store
            .cut_over_schema_migration(&tenant, &job_id, ready.fence, "validation-1")
            .await
            .expect("cut over migration");
        assert_eq!(pointer.bundle_digest, target_digest);
        assert_eq!(pointer.predecessor_digest, Some(source_digest));
        assert_eq!(
            store
                .page_schema_migration_shadow(&tenant, &job_id, None, 2)
                .await
                .expect("page migration shadow"),
            vec![row]
        );

        let racing_scope = SchemaScope {
            kind: SchemaScopeKind::Task,
            id: format!("racing-task-{suffix}"),
        };
        let first_digest = format!("sha256:{}", "a".repeat(64));
        let second_digest = format!("sha256:{}", "b".repeat(64));
        for (key, digest, request) in [
            (
                "race-first",
                &first_digest,
                format!("sha256:{}", "c".repeat(64)),
            ),
            (
                "race-second",
                &second_digest,
                format!("sha256:{}", "d".repeat(64)),
            ),
        ] {
            store
                .submit_schema_bundle(submission(&tenant, &racing_scope, key, &request, digest))
                .await
                .expect("submit racing root");
            verify(&store, &tenant, &racing_scope, digest, &request, key).await;
        }
        let first = ActivateSchemaBundle {
            tenant: tenant.clone(),
            scope: racing_scope.clone(),
            bundle_digest: first_digest,
            expected_predecessor: None,
            expected_fence: 1,
            verification_receipt_id: "race-first".into(),
            operation: operation("race-activate-first"),
        };
        let second = ActivateSchemaBundle {
            tenant: tenant.clone(),
            scope: racing_scope,
            bundle_digest: second_digest,
            expected_predecessor: None,
            expected_fence: 1,
            verification_receipt_id: "race-second".into(),
            operation: operation("race-activate-second"),
        };
        let (first_result, second_result) = tokio::join!(
            store.activate_schema_bundle(first),
            store.activate_schema_bundle(second)
        );
        assert!(matches!(
            (&first_result, &second_result),
            (Ok(_), Err(SchemaDeploymentStoreError::PredecessorMismatch))
                | (Err(SchemaDeploymentStoreError::PredecessorMismatch), Ok(_))
        ));

        let replay_scope = SchemaScope {
            kind: SchemaScopeKind::Task,
            id: format!("replay-task-{suffix}"),
        };
        let replay_digest = format!("sha256:{}", "e".repeat(64));
        let replay_request = format!("sha256:{}", "f".repeat(64));
        store
            .submit_schema_bundle(submission(
                &tenant,
                &replay_scope,
                "replay-submit",
                &replay_request,
                &replay_digest,
            ))
            .await
            .expect("submit replay root");
        verify(
            &store,
            &tenant,
            &replay_scope,
            &replay_digest,
            &replay_request,
            "replay-verify",
        )
        .await;
        let replay_command = ActivateSchemaBundle {
            tenant: tenant.clone(),
            scope: replay_scope,
            bundle_digest: replay_digest,
            expected_predecessor: None,
            expected_fence: 1,
            verification_receipt_id: "replay-verify".into(),
            operation: operation("concurrent-replay"),
        };
        let (left, right) = tokio::join!(
            store.activate_schema_bundle(replay_command.clone()),
            store.activate_schema_bundle(replay_command)
        );
        assert!(left.is_ok() && right.is_ok());
        assert!(matches!(
            (&left, &right),
            (
                Ok(ActivateSchemaBundleOutcome::Activated(_)),
                Ok(ActivateSchemaBundleOutcome::Replayed(_))
            ) | (
                Ok(ActivateSchemaBundleOutcome::Replayed(_)),
                Ok(ActivateSchemaBundleOutcome::Activated(_))
            )
        ));
    });
}
