use std::time::Duration;

use super::*;

#[test]
fn existing_source_append_serializes_with_migration_cutover() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("skipping PostgreSQL migration fence race: DATABASE_URL is not set");
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
        let tenant = format!("migration-fence-race-{suffix}");
        let scope = scope(&format!("task-{suffix}"));
        let source_digest = format!("sha256:{}", "1".repeat(64));
        let source_request = format!("sha256:{}", "3".repeat(64));

        store
            .submit_schema_bundle(submission(
                &tenant,
                &scope,
                "race-source-submit",
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
            "race-source-verify",
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
                    verification_receipt_id: "race-source-verify".into(),
                    operation: operation("race-source-activate"),
                })
                .await
                .expect("activate source"),
        );
        let source_pin = SchemaExecutionPin {
            scope: scope.clone(),
            bundle_digest: source_digest.clone(),
        };
        let journal_entity_id =
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                "entity-race",
                &source_pin,
            );
        let persistence_id = format!("{tenant}:Example.Task:{journal_entity_id}");
        store
            .append(
                &persistence_id,
                0,
                &[test_event(1, "Created", &persistence_id)],
            )
            .await
            .expect("append source event");

        let first = prepare_ready_migration(
            &store,
            &tenant,
            &scope,
            &source_digest,
            source_pointer.fence,
            "a",
            '2',
        )
        .await;
        let second = prepare_ready_migration(
            &store,
            &tenant,
            &scope,
            &source_digest,
            source_pointer.fence,
            "z",
            '9',
        )
        .await;
        assert!(
            first.job_id.as_str() < second.job_id.as_str(),
            "fixture must cut over the second locked row"
        );
        let job_id = second.job_id;
        let ready_fence = second.fence;
        let validation_id = second.validation_id;

        let mut append_tx = store
            .pool()
            .begin()
            .await
            .expect("begin append transaction");
        crate::store::assert_scoped_journal_write_fence(
            &mut append_tx,
            &tenant,
            "Example.Task",
            &journal_entity_id,
        )
        .await
        .expect("ready migration permits and locks pre-cutover append");
        let cutover_store = store.clone();
        let cutover_tenant = tenant.clone();
        let cutover_job = job_id.clone();
        let mut cutover = tokio::spawn(async move {
            cutover_store
                .cut_over_schema_migration(
                    &cutover_tenant,
                    &cutover_job,
                    ready_fence,
                    &validation_id,
                )
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut cutover)
                .await
                .is_err(),
            "cutover must wait for the append transaction's shared migration fence"
        );
        let segment_index: i64 = sqlx::query_scalar(
            "SELECT segment_index FROM events
             WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sequence_nr = 1",
        )
        .bind(&tenant)
        .bind("Example.Task")
        .bind(&journal_entity_id)
        .fetch_one(&mut *append_tx)
        .await
        .expect("load source segment");
        let event = test_event(2, "Timeout", &persistence_id);
        sqlx::query(
            "INSERT INTO events
             (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&tenant)
        .bind("Example.Task")
        .bind(&journal_entity_id)
        .bind(2_i64)
        .bind(segment_index)
        .bind(&event.event_type)
        .bind(&event.payload)
        .bind(serde_json::to_value(&event.metadata).expect("encode metadata"))
        .execute(&mut *append_tx)
        .await
        .expect("append source event while holding fence");
        append_tx.commit().await.expect("commit source append");

        assert_eq!(
            cutover
                .await
                .expect("cutover task join")
                .expect_err("cutover must revalidate after the append commits"),
            SchemaDeploymentStoreError::StaleFence
        );
        assert_eq!(
            store
                .active_schema_pointer(&tenant, &scope)
                .await
                .expect("load active pointer")
                .expect("source remains active")
                .bundle_digest,
            source_pin.bundle_digest
        );
    });
}

fn test_event(sequence_nr: u64, event_type: &str, actor_id: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr,
        event_type: event_type.into(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id: temper_runtime::scheduler::sim_uuid(),
            causation_id: temper_runtime::scheduler::sim_uuid(),
            correlation_id: temper_runtime::scheduler::sim_uuid(),
            timestamp: temper_runtime::scheduler::sim_now(),
            actor_id: actor_id.into(),
        },
    }
}

struct ReadyMigration {
    job_id: String,
    fence: u64,
    validation_id: String,
}

async fn prepare_ready_migration(
    store: &PostgresEventStore,
    tenant: &str,
    scope: &SchemaScope,
    source_digest: &str,
    source_fence: u64,
    key: &str,
    digest_digit: char,
) -> ReadyMigration {
    let target_digest = format!("sha256:{}", digest_digit.to_string().repeat(64));
    let target_request = format!("sha256:{}", key.repeat(64));
    let module_digest = format!("sha256:{}", "5".repeat(64));
    let verify_id = format!("race-target-verify-{key}");
    let mut target = submission(
        tenant,
        scope,
        &format!("race-target-submit-{key}"),
        &target_request,
        &target_digest,
    );
    target.bundle.predecessor_digest = Some(source_digest.into());
    target.bundle.migration_module_name = Some("reshape".into());
    target.bundle.migration_module_digest = Some(module_digest.clone());
    target.bundle.migration_abi_version = Some("temper-schema-migration/v1".into());
    store
        .submit_schema_bundle(target)
        .await
        .expect("submit target");
    verify(
        store,
        tenant,
        scope,
        &target_digest,
        &target_request,
        &verify_id,
    )
    .await;
    let job_id = format!("migration-fence-race-{key}");
    store
        .create_schema_migration(CreateSchemaMigration {
            job_id: job_id.clone(),
            tenant: tenant.into(),
            scope: scope.clone(),
            source_bundle_digest: source_digest.into(),
            target_bundle_digest: target_digest,
            verification_receipt_id: verify_id,
            source_expected_fence: source_fence,
            module_name: "reshape".into(),
            module_digest,
            accepted_authority_json: r#"{"principal":"race-test"}"#.into(),
            budgets: SchemaMigrationBudgets {
                fuel_per_entity: 10_000,
                memory_pages: 2,
                input_bytes: 4_096,
                output_bytes: 4_096,
                entities_per_batch: 1,
                total_entities: 1,
                total_batches: 1,
                attempts: 1,
            },
            idempotency_key: format!("migration-fence-race-{key}"),
            request_digest: format!("sha256:{}", "6".repeat(64)),
            request_id: format!("migration-fence-race-{key}"),
        })
        .await
        .expect("create migration");
    let claimed = store
        .claim_schema_migration(tenant, &job_id, 1, 2)
        .await
        .expect("claim migration");
    let validating = store
        .commit_schema_migration_batch(
            tenant,
            CommitSchemaMigrationBatch {
                job_id: job_id.clone(),
                expected_fence: claimed.fence,
                expected_cursor: None,
                next_cursor: None,
                scan_complete: true,
                restart_scan: false,
                observed_source_write_version: 1,
                rows: Vec::new(),
                receipt: SchemaMigrationBatchReceipt {
                    id: format!("race-empty-batch-{key}"),
                    source_cursor: None,
                    next_cursor: None,
                    input_digest: format!("sha256:{}", "7".repeat(64)),
                    output_digest: format!("sha256:{}", "8".repeat(64)),
                    row_count: 0,
                },
            },
        )
        .await
        .expect("commit empty migration scan");
    let validation_id = format!("race-validation-{key}");
    let ready = store
        .validate_schema_migration(
            tenant,
            &job_id,
            validating.fence,
            SchemaMigrationValidationReceipt {
                id: validation_id.clone(),
                shadow_digest: format!("sha256:{}", "8".repeat(64)),
                caught_up_sequence: 1,
                passed: true,
            },
        )
        .await
        .expect("validate migration");
    ReadyMigration {
        job_id,
        fence: ready.fence,
        validation_id,
    }
}
