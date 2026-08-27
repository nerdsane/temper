use super::*;
use sha2::{Digest as _, Sha256};
use temper_runtime::persistence::{
    EventMetadata, KernelEventMetadata, PersistenceEnvelope, StreamDescriptorInputV1,
    StreamDescriptorV1, StreamEntityRef, StreamMutability, StreamStorageRefV1,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};

async fn wasm_stream_call(
    invocation: &ApplicationDataInvocation,
    operation: SchemaDeploymentOperationV1,
) -> SchemaDeploymentResponseV1 {
    let encoded = serde_json::to_vec(&SchemaDeploymentRequestV1 {
        abi: SCHEMA_DEPLOYMENT_ABI_V1.into(),
        operation,
    })
    .unwrap();
    serde_json::from_slice(&invocation.call_encoded(&encoded).await.unwrap()).unwrap()
}

fn stream_bundle_request(
    csdl: &str,
    predecessor_digest: Option<String>,
    key: &str,
) -> SubmitSchemaBundleRequestV1 {
    let budgets = ScopedBundleBudgets::default();
    let compiled = ScopedSpecBundle::compile(ScopedSpecBundleInput {
        scope_id: "stream-task".into(),
        predecessor_digest: predecessor_digest.clone(),
        csdl_xml: csdl.into(),
        ioa_sources: vec![IoaSourceInput {
            entity_type: "Example.File".into(),
            source: STREAM_IOA.into(),
        }],
        cedar_policies: vec![],
        wasm_modules: vec![],
        migration: None,
        budgets: budgets.clone(),
    })
    .unwrap();
    SubmitSchemaBundleRequestV1 {
        request_id: format!("{key}-request"),
        idempotency_key: key.into(),
        scope: SchemaScopeV1 {
            kind: "task".into(),
            id: "stream-task".into(),
        },
        expected_predecessor: predecessor_digest,
        expected_digest: compiled.digest().into(),
        canonicalization_version: temper_spec::bundle::SCOPED_SPEC_BUNDLE_CONTRACT_V1.into(),
        csdl: csdl.into(),
        ioa: vec![SchemaIoaSourceV1 {
            entity_type: "Example.File".into(),
            source: STREAM_IOA.into(),
        }],
        cedar_policies: vec![],
        wasm_modules: vec![],
        migration: None,
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
    }
}

async fn verify_bundle(
    state: &crate::ServerState,
    digest: &str,
    key: &str,
    logical_now: u64,
) -> u64 {
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "stream-task".into(),
    };
    let store = state
        .storage_stack
        .as_ref()
        .unwrap()
        .schema_deployments
        .as_ref()
        .unwrap();
    let receipt_id = format!("{key}-receipt");
    let claim = match store
        .claim_schema_verification(ClaimSchemaVerification {
            tenant: "default".into(),
            scope: scope.clone(),
            bundle_digest: digest.into(),
            logical_now,
            lease_expires_at: logical_now + 10,
            operation: SchemaOperationIdentity {
                idempotency_key: format!("{key}-claim"),
                request_digest: format!("sha256:{}", "4".repeat(64)),
                request_id: format!("{key}-claim"),
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
            digest,
            claim.fence,
            SchemaVerificationReceipt {
                id: receipt_id,
                verifier_version: "test/v1".into(),
                input_digest: format!("sha256:{}", "5".repeat(64)),
                passed: true,
            },
        )
        .await
        .unwrap()
        .fence
}

fn activation(
    digest: String,
    predecessor: Option<String>,
    fence: u64,
    key: &str,
) -> ActivateSchemaBundleRequestV1 {
    ActivateSchemaBundleRequestV1 {
        request_id: format!("{key}-request"),
        idempotency_key: key.into(),
        scope: SchemaScopeV1 {
            kind: "task".into(),
            id: "stream-task".into(),
        },
        bundle_digest: digest,
        expected_predecessor: predecessor,
        expected_fence: fence,
        verification_receipt_id: format!("{key}-receipt"),
        stream_descriptor_completion_receipt_id: None,
    }
}

#[tokio::test]
async fn governed_descriptor_migration_repairs_replays_and_atomically_fences_activation() {
    let (invocation, durable_store) = schema_invocation_with_store();
    let state = &invocation.state;
    let service = crate::schema_deployment::GovernedSchemaDeploymentService::new(state);
    let security = SecurityContext::system();
    let inactive_csdl = ACTIVE_STREAM_CSDL.replace(
        "<Annotation Term=\"Temper.Vocab.Stream.DescriptorContractVersion\" Int=\"1\"/>",
        "",
    );

    let source = service
        .submit(
            "default",
            &security,
            stream_bundle_request(&inactive_csdl, None, "stream-source-submit"),
        )
        .await
        .unwrap();
    let source_fence = verify_bundle(state, &source.bundle_digest, "stream-source", 1).await;
    service
        .activate(
            "default",
            &security,
            activation(
                source.bundle_digest.clone(),
                None,
                source_fence,
                "stream-source",
            ),
        )
        .await
        .unwrap();

    let body = b"historical governed bytes";
    let content_hash = format!("sha256:{:x}", Sha256::digest(body));
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "stream-task".into(),
    };
    let journal_entity_id =
        temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
            "legacy-file",
            &SchemaExecutionPin {
                scope: scope.clone(),
                bundle_digest: source.bundle_digest.clone(),
            },
        );
    let persistence_id = format!("default:File:{journal_entity_id}");
    let mut historical_payload = serde_json::json!({
        "action": "StreamUpdated",
        "from_status": "Ready",
        "to_status": "Ready",
        "timestamp": sim_now(),
        "params": {
            "content_hash": content_hash,
            "size_bytes": body.len() as u64,
            "mime_type": "text/plain"
        },
        "idempotency_key": null
    });
    historical_payload.as_object_mut().unwrap().insert(
        crate::entity_actor::SCHEMA_PIN_FIELD.into(),
        serde_json::to_value(crate::entity_actor::schema_event_pin(
            &SchemaExecutionPin {
                scope: scope.clone(),
                bundle_digest: source.bundle_digest.clone(),
            },
            "File",
            "StreamUpdated",
        ))
        .unwrap(),
    );
    let historical = PersistenceEnvelope {
        sequence_nr: 1,
        event_type: "StreamUpdated".into(),
        payload: historical_payload,
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: persistence_id.clone(),
            kernel: None,
        },
    };
    let journal = state.event_journal().unwrap().0;
    journal
        .append(&persistence_id, 0, &[historical])
        .await
        .unwrap();

    let target = service
        .submit(
            "default",
            &security,
            stream_bundle_request(
                ACTIVE_STREAM_CSDL,
                Some(source.bundle_digest.clone()),
                "stream-target-submit",
            ),
        )
        .await
        .unwrap();
    let target_fence = verify_bundle(state, &target.bundle_digest, "stream-target", 2).await;
    let target_activation = activation(
        target.bundle_digest.clone(),
        Some(source.bundle_digest.clone()),
        target_fence,
        "stream-target",
    );
    let error = service
        .activate("default", &security, target_activation.clone())
        .await
        .unwrap_err();
    assert_eq!(error.into_contract().code, "migration_required");

    let capabilities = temper_spec::csdl::verify_stream_capabilities_v1(
        &temper_spec::parse_csdl(ACTIVE_STREAM_CSDL).unwrap(),
    )
    .unwrap();
    let start_request = StartStreamDescriptorMigrationRequestV1 {
        request_id: "stream-migration-start".into(),
        idempotency_key: "stream-migration-start".into(),
        target: StreamDescriptorMigrationTargetV1::TaskBundle {
            scope: target_activation.scope.clone(),
            bundle_digest: target.bundle_digest.clone(),
        },
        expected_capability_digest: temper_spec::csdl::stream_capability_set_digest_v1(
            &capabilities,
        )
        .unwrap(),
        descriptor_contract_version: 1,
        budgets: StreamDescriptorMigrationBudgetsV1 {
            max_subjects: 16,
            max_events_per_subject: 64,
            max_blob_bytes: 1_048_576,
        },
    };
    let started = service
        .start_stream_descriptor_migration("default", &security, start_request.clone())
        .await
        .unwrap();
    let mut invalid_start = start_request.clone();
    invalid_start.request_id.clear();
    invalid_start.idempotency_key = " ".into();
    let invalid_http_start = super::super::tests::authenticated_router(
        invocation.state.clone(),
        SecurityContext::system(),
    )
    .oneshot(
        Request::post("/api/v1/schema-deployments/stream-descriptor-migrations")
            .header("content-type", "application/json")
            .header("x-tenant-id", "default")
            .body(Body::from(serde_json::to_vec(&invalid_start).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(invalid_http_start.status(), StatusCode::BAD_REQUEST);
    let invalid_wasm = wasm_stream_call(
        &invocation,
        SchemaDeploymentOperationV1::GetStreamDescriptorMigration(
            GetStreamDescriptorMigrationRequestV1 {
                request_id: "invalid-job-id".into(),
                job_id: "caller-minted".into(),
            },
        ),
    )
    .await;
    let SchemaDeploymentResponseV1::Error { error } = invalid_wasm else {
        panic!("typed WASM must reject a caller-minted migration job id")
    };
    assert_eq!(error.code, "invalid_bundle");
    let denied = service
        .get_stream_descriptor_migration(
            "default",
            &SecurityContext::anonymous(),
            GetStreamDescriptorMigrationRequestV1 {
                request_id: "stream-migration-denied".into(),
                job_id: started.job_id.clone(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(denied.into_contract().code, "authorization_denied");
    let cross_tenant = service
        .get_stream_descriptor_migration(
            "other-tenant",
            &security,
            GetStreamDescriptorMigrationRequestV1 {
                request_id: "stream-migration-cross-tenant".into(),
                job_id: started.job_id.clone(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(cross_tenant.into_contract().code, "invalid_bundle");
    let http_start = super::super::tests::authenticated_router(
        invocation.state.clone(),
        SecurityContext::system(),
    )
    .oneshot(
        Request::post("/api/v1/schema-deployments/stream-descriptor-migrations")
            .header("content-type", "application/json")
            .header("x-tenant-id", "default")
            .body(Body::from(serde_json::to_vec(&start_request).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(http_start.status(), StatusCode::OK);
    let SchemaDeploymentResponseV1::StreamDescriptorMigration {
        receipt: wasm_started,
    } = wasm_stream_call(
        &invocation,
        SchemaDeploymentOperationV1::StartStreamDescriptorMigration(start_request.clone()),
    )
    .await
    else {
        panic!("typed WASM migration start should succeed")
    };
    assert_eq!(wasm_started, started);
    let replayed_start = service
        .start_stream_descriptor_migration(
            "default",
            &security,
            StartStreamDescriptorMigrationRequestV1 {
                request_id: "ignored-replay-request-id".into(),
                ..start_request
            },
        )
        .await
        .unwrap();
    assert_eq!(replayed_start, started);

    let missing_request = AdvanceStreamDescriptorMigrationRequestV1 {
        request_id: "stream-migration-missing".into(),
        idempotency_key: "stream-migration-missing".into(),
        job_id: started.job_id.clone(),
    };
    let missing = service
        .advance_stream_descriptor_migration("default", &security, missing_request.clone())
        .await
        .unwrap();
    assert_eq!(missing.status, "unresolved");
    assert_eq!(missing.unresolved_subjects, 1);
    let http_advance = super::super::tests::authenticated_router(
        invocation.state.clone(),
        SecurityContext::system(),
    )
    .oneshot(
        Request::post(format!(
            "/api/v1/schema-deployments/stream-descriptor-migrations/{}/advance",
            started.job_id
        ))
        .header("content-type", "application/json")
        .header("x-tenant-id", "default")
        .body(Body::from(serde_json::to_vec(&missing_request).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(http_advance.status(), StatusCode::OK);
    let SchemaDeploymentResponseV1::StreamDescriptorMigration {
        receipt: wasm_advanced,
    } = wasm_stream_call(
        &invocation,
        SchemaDeploymentOperationV1::AdvanceStreamDescriptorMigration(missing_request.clone()),
    )
    .await
    else {
        panic!("typed WASM migration advance should succeed")
    };
    assert_eq!(wasm_advanced, missing);
    let SchemaDeploymentResponseV1::StreamDescriptorMigration { receipt: wasm_get } =
        wasm_stream_call(
            &invocation,
            SchemaDeploymentOperationV1::GetStreamDescriptorMigration(
                GetStreamDescriptorMigrationRequestV1 {
                    request_id: "stream-migration-wasm-get".into(),
                    job_id: started.job_id.clone(),
                },
            ),
        )
        .await
    else {
        panic!("typed WASM migration get should succeed")
    };
    assert_eq!(wasm_get.status, "unresolved");
    let SchemaDeploymentResponseV1::UnresolvedStreamDescriptors {
        page: wasm_unresolved,
    } = wasm_stream_call(
        &invocation,
        SchemaDeploymentOperationV1::ListUnresolvedStreamDescriptors(
            ListUnresolvedStreamDescriptorsRequestV1 {
                request_id: "stream-migration-wasm-unresolved".into(),
                job_id: started.job_id.clone(),
                after: None,
                limit: 16,
            },
        ),
    )
    .await
    else {
        panic!("typed WASM unresolved list should succeed")
    };
    assert_eq!(wasm_unresolved.entries.len(), 1);

    let http_get = super::super::tests::authenticated_router(
        invocation.state.clone(),
        SecurityContext::system(),
    )
    .oneshot(
        Request::get(format!(
            "/api/v1/schema-deployments/stream-descriptor-migrations/{}",
            started.job_id
        ))
        .header("x-tenant-id", "default")
        .body(Body::empty())
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(http_get.status(), StatusCode::OK);
    let unresolved_request = ListUnresolvedStreamDescriptorsRequestV1 {
        request_id: "stream-migration-http-unresolved".into(),
        job_id: started.job_id.clone(),
        after: None,
        limit: 16,
    };
    let http_unresolved = super::super::tests::authenticated_router(
        invocation.state.clone(),
        SecurityContext::system(),
    )
    .oneshot(
        Request::post(format!(
            "/api/v1/schema-deployments/stream-descriptor-migrations/{}/unresolved",
            started.job_id
        ))
        .header("content-type", "application/json")
        .header("x-tenant-id", "default")
        .body(Body::from(serde_json::to_vec(&unresolved_request).unwrap()))
        .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(http_unresolved.status(), StatusCode::OK);
    let mut restarted_state = crate::ServerState::from_registry_shared(
        temper_runtime::ActorSystem::new("stream-migration-restart"),
        invocation.state.registry.clone(),
    );
    restarted_state.data_dir = tempfile::tempdir().unwrap().keep();
    restarted_state.set_storage_stack(crate::storage::StorageStack::from_sim(
        durable_store.clone(),
        None,
    ));
    let restarted_service =
        crate::schema_deployment::GovernedSchemaDeploymentService::new(&restarted_state);
    let restarted = restarted_service
        .get_stream_descriptor_migration(
            "default",
            &security,
            GetStreamDescriptorMigrationRequestV1 {
                request_id: "stream-migration-after-restart".into(),
                job_id: started.job_id.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(restarted.status, "unresolved");
    let replayed_missing = service
        .advance_stream_descriptor_migration(
            "default",
            &security,
            AdvanceStreamDescriptorMigrationRequestV1 {
                request_id: "ignored-advance-replay-request-id".into(),
                ..missing_request
            },
        )
        .await
        .unwrap();
    assert_eq!(replayed_missing, missing);

    durable_store.fail_next_reads(&persistence_id, 1);
    let transient_read_failure = service
        .advance_stream_descriptor_migration(
            "default",
            &security,
            AdvanceStreamDescriptorMigrationRequestV1 {
                request_id: "stream-migration-transient-read".into(),
                idempotency_key: "stream-migration-transient-read".into(),
                job_id: started.job_id.clone(),
            },
        )
        .await
        .unwrap_err()
        .into_contract();
    assert_eq!(transient_read_failure.code, "backend_unavailable");
    assert!(transient_read_failure.retryable);

    state
        .put_blob_object(
            &TenantId::default(),
            &format!("temper-fs/{content_hash}"),
            body,
            None,
        )
        .await
        .unwrap();
    let repaired = service
        .advance_stream_descriptor_migration(
            "default",
            &security,
            AdvanceStreamDescriptorMigrationRequestV1 {
                request_id: "stream-migration-repair".into(),
                idempotency_key: "stream-migration-repair".into(),
                job_id: started.job_id.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(repaired.unresolved_subjects, 0);
    assert_eq!(repaired.status, "migrating");
    assert_eq!(repaired.page_outcomes[0].classification, "migrated");

    let completed = service
        .advance_stream_descriptor_migration(
            "default",
            &security,
            AdvanceStreamDescriptorMigrationRequestV1 {
                request_id: "stream-migration-stable-pass".into(),
                idempotency_key: "stream-migration-stable-pass".into(),
                job_id: started.job_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.page_outcomes[0].classification, "already_present");

    let events = journal.read_events(&persistence_id, 1).await.unwrap();
    let descriptor = events
        .last()
        .and_then(|event| event.metadata.kernel.as_ref())
        .map(|kernel| kernel.stream_descriptor())
        .unwrap();
    assert_eq!(descriptor.content_hash(), content_hash);
    assert_eq!(descriptor.byte_length(), body.len() as u64);
    assert!(descriptor.authorization_parent().is_none());

    durable_store.fail_next_reads(
        &format!("default:_TemperStreamMigration:{}", completed.job_id),
        1,
    );
    let activation_backend_failure = service
        .activate(
            "default",
            &security,
            ActivateSchemaBundleRequestV1 {
                request_id: "stream-activate-backend-failure".into(),
                idempotency_key: "stream-activate-backend-failure".into(),
                stream_descriptor_completion_receipt_id: completed.completion_receipt_id.clone(),
                ..target_activation.clone()
            },
        )
        .await
        .unwrap_err()
        .into_contract();
    assert_eq!(activation_backend_failure.code, "backend_unavailable");
    assert!(activation_backend_failure.retryable);

    let late_body = b"publication after completion";
    let late_hash = format!("sha256:{:x}", Sha256::digest(late_body));
    state
        .put_blob_object(
            &TenantId::default(),
            &format!("temper-fs/{late_hash}"),
            late_body,
            None,
        )
        .await
        .unwrap();
    let mut late_payload = serde_json::json!({
        "action": "StreamUpdated",
        "from_status": "Ready",
        "to_status": "Ready",
        "timestamp": sim_now(),
        "params": {
            "content_hash": late_hash,
            "size_bytes": late_body.len() as u64,
            "mime_type": "text/plain"
        },
        "idempotency_key": null
    });
    late_payload.as_object_mut().unwrap().insert(
        crate::entity_actor::SCHEMA_PIN_FIELD.into(),
        serde_json::to_value(crate::entity_actor::schema_event_pin(
            &SchemaExecutionPin {
                scope: scope.clone(),
                bundle_digest: source.bundle_digest.clone(),
            },
            "File",
            "StreamUpdated",
        ))
        .unwrap(),
    );
    journal
        .append(
            &persistence_id,
            2,
            &[PersistenceEnvelope {
                sequence_nr: 3,
                event_type: "StreamUpdated".into(),
                payload: late_payload,
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: persistence_id.clone(),
                    kernel: None,
                },
            }],
        )
        .await
        .unwrap();
    let stale_activation = service
        .activate(
            "default",
            &security,
            ActivateSchemaBundleRequestV1 {
                request_id: "stream-activate-stale-evidence".into(),
                idempotency_key: "stream-activate-stale-evidence".into(),
                stream_descriptor_completion_receipt_id: completed.completion_receipt_id.clone(),
                ..target_activation.clone()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(stale_activation.into_contract().code, "migration_required");
    let reopened = service
        .advance_stream_descriptor_migration(
            "default",
            &security,
            AdvanceStreamDescriptorMigrationRequestV1 {
                request_id: "stream-migration-reopen".into(),
                idempotency_key: "stream-migration-reopen".into(),
                job_id: completed.job_id.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(reopened.status, "migrating");
    let recompleted = service
        .advance_stream_descriptor_migration(
            "default",
            &security,
            AdvanceStreamDescriptorMigrationRequestV1 {
                request_id: "stream-migration-recomplete".into(),
                idempotency_key: "stream-migration-recomplete".into(),
                job_id: completed.job_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(recompleted.status, "completed");

    let successful_activation = ActivateSchemaBundleRequestV1 {
        request_id: "stream-activate-with-evidence".into(),
        idempotency_key: "stream-activate-with-evidence".into(),
        stream_descriptor_completion_receipt_id: recompleted.completion_receipt_id,
        ..target_activation
    };
    service
        .activate("default", &security, successful_activation.clone())
        .await
        .unwrap();
    durable_store
        .fail_next_schema_operations(temper_store_sim::SimSchemaFaultPoint::ActivePointerRead, 1);
    let source_pin = SchemaExecutionPin {
        scope: scope.clone(),
        bundle_digest: source.bundle_digest.clone(),
    };
    let fence_read_failure = state
        .stream_descriptor_contract_activated(&TenantId::default(), Some(&source_pin), "File")
        .await
        .unwrap_err();
    assert_eq!(
        fence_read_failure.stable_code(),
        "StreamDescriptorUnavailable"
    );

    let mut agent_ctx = crate::request_context::AgentContext::system();
    agent_ctx.schema_pin = Some(SchemaExecutionPin {
        scope: scope.clone(),
        bundle_digest: source.bundle_digest.clone(),
    });
    state
        .dispatch(crate::state::DispatchCommand {
            tenant: &TenantId::default(),
            entity_type: "File",
            entity_id: "legacy-file",
            action: "Touch",
            params: serde_json::json!({}),
            agent_ctx: &agent_ctx,
            await_integration: false,
            await_reactions: false,
        })
        .await
        .unwrap();
    service
        .activate("default", &security, successful_activation)
        .await
        .unwrap();

    let replacement_body = b"post-activation mutable bytes";
    let replacement_hash = format!("sha256:{:x}", Sha256::digest(replacement_body));
    state
        .put_blob_object(
            &TenantId::default(),
            &format!("temper-fs/{replacement_hash}"),
            replacement_body,
            None,
        )
        .await
        .unwrap();
    let replacement_descriptor = StreamDescriptorV1::new(StreamDescriptorInputV1 {
        subject: StreamEntityRef::new("File", "legacy-file").unwrap(),
        authorization_parent: None,
        content_hash: replacement_hash.clone(),
        storage: StreamStorageRefV1::new(format!("temper-fs/{replacement_hash}")).unwrap(),
        byte_length: replacement_body.len() as u64,
        content_type: Some("text/plain".into()),
        content_event_sequence: 6,
        descriptor_event_sequence: 6,
        mutability: StreamMutability::Mutable,
    })
    .unwrap();
    state
        .dispatch_typed_checked_with_kernel(
            crate::state::DispatchCommand {
                tenant: &TenantId::default(),
                entity_type: "File",
                entity_id: "legacy-file",
                action: "StreamUpdated",
                params: serde_json::json!({
                    "content_hash": replacement_hash,
                    "size_bytes": replacement_body.len() as u64,
                    "mime_type": "text/plain"
                }),
                agent_ctx: &agent_ctx,
                await_integration: false,
                await_reactions: false,
            },
            None,
            Some(KernelEventMetadata::V1 {
                stream_descriptor: replacement_descriptor,
            }),
        )
        .await
        .unwrap();

    let mut stale_publication = events[0].clone();
    stale_publication.sequence_nr = 7;
    stale_publication.event_type = "StreamUpdated".into();
    stale_publication.payload["action"] = serde_json::Value::String("StreamUpdated".into());
    stale_publication.metadata.kernel = None;
    assert!(
        journal
            .append(&persistence_id, 6, &[stale_publication])
            .await
            .is_err()
    );
}
