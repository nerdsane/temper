use super::*;

use temper_runtime::ActorSystem;
use temper_wasm_sdk::schema_deployment::BootstrapFailureStageV1;
use temper_wasm_sdk::schema_deployment::{BootstrapActionV1, BootstrapDispatchRequestV1};

use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::storage::StorageStack;

mod admission;
mod fixture;
use fixture::bootstrap_bundle_request;

async fn call_bootstrap(
    invocation: &ApplicationDataInvocation,
    request: BootstrapDispatchRequestV1,
) -> SchemaDeploymentResponseV1 {
    let encoded = serde_json::to_vec(&SchemaDeploymentRequestV1 {
        abi: SCHEMA_DEPLOYMENT_ABI_V1.into(),
        operation: SchemaDeploymentOperationV1::BootstrapDispatch(request),
    })
    .expect("bootstrap request should encode");
    serde_json::from_slice(
        &invocation
            .call_encoded(&encoded)
            .await
            .expect("bootstrap host call should return a typed response"),
    )
    .expect("bootstrap response should decode")
}

#[tokio::test]
async fn bootstrap_dispatch_creates_actions_and_replays_exact_receipt_after_cold_host_restart() {
    let original = invocation(
        BTreeSet::from([DataOperationKind::SchemaBootstrapDispatch]),
        SecurityContext::system(),
    );
    let mut state = original.state.clone();
    state.data_dir = tempfile::tempdir().unwrap().keep();
    let store = temper_store_sim::SimEventStore::no_faults(780);
    state.storage_stack = Some(std::sync::Arc::new(StorageStack::from_sim(
        store.clone(),
        None,
    )));
    let invocation = ApplicationDataInvocation::new(state, original.authority.clone());
    let service = crate::schema_deployment::GovernedSchemaDeploymentService::new(&invocation.state);
    let security = SecurityContext::system();
    let submit = bootstrap_bundle_request();
    let submitted = service
        .submit("default", &security, submit.clone())
        .await
        .expect("bootstrap bundle should submit");
    let internal_scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "bootstrap-e2e".into(),
    };
    let schema_store = invocation
        .state
        .storage_stack
        .as_ref()
        .unwrap()
        .schema_deployments
        .as_ref()
        .unwrap();
    let claimed = schema_store
        .claim_schema_verification(ClaimSchemaVerification {
            tenant: "default".into(),
            scope: internal_scope.clone(),
            bundle_digest: submitted.bundle_digest.clone(),
            logical_now: 1,
            lease_expires_at: 10,
            operation: SchemaOperationIdentity {
                idempotency_key: "bootstrap-e2e-verify".into(),
                request_digest: format!("sha256:{}", "7".repeat(64)),
                request_id: "bootstrap-e2e-verify".into(),
            },
        })
        .await
        .expect("bootstrap verification should claim");
    let claimed = match claimed {
        ClaimSchemaVerificationOutcome::Claimed(record) => record,
        ClaimSchemaVerificationOutcome::Replayed(_) => panic!("first verification must claim"),
    };
    let verified = schema_store
        .finish_schema_verification(
            "default",
            &internal_scope,
            &submitted.bundle_digest,
            claimed.fence,
            SchemaVerificationReceipt {
                id: "bootstrap-e2e-verification-receipt".into(),
                verifier_version: "bootstrap-e2e/v1".into(),
                input_digest: format!("sha256:{}", "8".repeat(64)),
                passed: true,
            },
        )
        .await
        .expect("bootstrap verification receipt should persist");
    service
        .activate(
            "default",
            &security,
            ActivateSchemaBundleRequestV1 {
                request_id: "bootstrap-e2e-activate".into(),
                idempotency_key: "bootstrap-e2e-activate".into(),
                scope: submit.scope,
                bundle_digest: submitted.bundle_digest.clone(),
                expected_predecessor: None,
                expected_fence: verified.fence,
                verification_receipt_id: verified
                    .verification_receipt_id
                    .expect("verification receipt should exist"),
                stream_descriptor_completion_receipt_id: None,
            },
        )
        .await
        .expect("bootstrap bundle should activate");

    let request = BootstrapDispatchRequestV1 {
        request_id: "bootstrap-e2e-request".into(),
        idempotency_key: "bootstrap-e2e-operation".into(),
        activation_request_id: "bootstrap-e2e-activate".into(),
        entity_type: "Temper.Example.Customer".into(),
        entity_id: "018f1f80-7b2d-7000-8000-000000000078".into(),
        initial_fields: serde_json::json!({"Name":"before"})
            .as_object()
            .cloned()
            .unwrap(),
        initial_action: Some(BootstrapActionV1 {
            action: "Rename".into(),
            parameters: serde_json::json!({"Name":"after"})
                .as_object()
                .cloned()
                .unwrap(),
        }),
    };
    let first = call_bootstrap(&invocation, request.clone()).await;
    let SchemaDeploymentResponseV1::Bootstrap { receipt } = first else {
        panic!("bootstrap should return a dedicated receipt: {first:?}")
    };
    assert_eq!(receipt.request_id, "bootstrap-e2e-request");
    assert_eq!(
        receipt.pin.scope,
        SchemaScopeV1 {
            kind: "task".into(),
            id: "bootstrap-e2e".into(),
        }
    );
    assert_eq!(receipt.pin.bundle_digest, submitted.bundle_digest);
    assert_eq!(receipt.creation_sequence, Some(1));
    assert_eq!(receipt.action_sequence, Some(2));
    assert_eq!(receipt.action_result.as_ref().unwrap()["Name"], "after");
    assert_eq!(receipt.action_result.as_ref().unwrap()["RenameCount"], 1);
    assert!(receipt.failure.is_none());

    let crash_request = BootstrapDispatchRequestV1 {
        request_id: "bootstrap-action-crash".into(),
        idempotency_key: "bootstrap-action-crash".into(),
        activation_request_id: "bootstrap-e2e-activate".into(),
        entity_type: "Temper.Example.Customer".into(),
        entity_id: "018f1f80-7b2d-7000-8000-000000000084".into(),
        initial_fields: serde_json::json!({"Name":"before"})
            .as_object()
            .cloned()
            .unwrap(),
        initial_action: Some(BootstrapActionV1 {
            action: "Rename".into(),
            parameters: serde_json::json!({"Name":"after"})
                .as_object()
                .cloned()
                .unwrap(),
        }),
    };
    store.fail_next_schema_operations(temper_store_sim::SimSchemaFaultPoint::CompleteBootstrap, 1);
    let interrupted = call_bootstrap(&invocation, crash_request.clone()).await;
    assert!(matches!(
        interrupted,
        SchemaDeploymentResponseV1::Error { .. }
    ));
    crate::application_data::GovernedApplicationDataService::new(&invocation.state)
        .action(
            &TenantId::default(),
            "Customer",
            &crash_request.entity_id,
            "Rename",
            serde_json::json!({"Name":"later"}),
            &AgentContext {
                security_ctx: Some(SecurityContext::system()),
                agent_id: Some("system".into()),
                idempotency_key: Some("later-action".into()),
                schema_pin: Some(SchemaExecutionPin {
                    scope: internal_scope.clone(),
                    bundle_digest: submitted.bundle_digest.clone(),
                }),
                ..AgentContext::default()
            },
        )
        .await
        .expect("a later action should alter current state after the crash window");
    let recovered_action = call_bootstrap(&invocation, crash_request).await;
    let SchemaDeploymentResponseV1::Bootstrap {
        receipt: recovered_action,
    } = recovered_action
    else {
        panic!("the interrupted coordinator should recover from its exact journal event")
    };
    assert_eq!(recovered_action.action_sequence, Some(2));
    assert_eq!(
        recovered_action.action_result.as_ref().unwrap()["Name"],
        "after"
    );
    assert_eq!(
        recovered_action.action_result.as_ref().unwrap()["RenameCount"],
        1
    );

    let rejected_request = BootstrapDispatchRequestV1 {
        request_id: "bootstrap-rejected-request".into(),
        idempotency_key: "bootstrap-rejected-operation".into(),
        activation_request_id: "bootstrap-e2e-activate".into(),
        entity_type: "Temper.Example.Customer".into(),
        entity_id: "018f1f80-7b2d-7000-8000-000000000080".into(),
        initial_fields: serde_json::Map::new(),
        initial_action: Some(BootstrapActionV1 {
            action: "Reject".into(),
            parameters: serde_json::Map::new(),
        }),
    };
    store.fail_next_schema_operations(temper_store_sim::SimSchemaFaultPoint::CompleteBootstrap, 1);
    let interrupted_rejection = call_bootstrap(&invocation, rejected_request.clone()).await;
    assert!(matches!(
        interrupted_rejection,
        SchemaDeploymentResponseV1::Error { .. }
    ));
    crate::application_data::GovernedApplicationDataService::new(&invocation.state)
        .action(
            &TenantId::default(),
            "Customer",
            &rejected_request.entity_id,
            "Disable",
            serde_json::json!({}),
            &AgentContext {
                security_ctx: Some(SecurityContext::system()),
                agent_id: Some("system".into()),
                idempotency_key: Some("later-disable".into()),
                schema_pin: Some(SchemaExecutionPin {
                    scope: internal_scope.clone(),
                    bundle_digest: submitted.bundle_digest.clone(),
                }),
                ..AgentContext::default()
            },
        )
        .await
        .expect("later state transition should make Reject newly enabled");
    let rejected = call_bootstrap(&invocation, rejected_request.clone()).await;
    let SchemaDeploymentResponseV1::Bootstrap {
        receipt: rejected_receipt,
    } = rejected
    else {
        panic!("guard rejection should return an authoritative partial receipt: {rejected:?}")
    };
    assert_eq!(rejected_receipt.creation_sequence, Some(1));
    assert_eq!(rejected_receipt.action_sequence, None);
    assert_eq!(
        rejected_receipt
            .failure
            .as_ref()
            .map(|failure| failure.stage),
        Some(BootstrapFailureStageV1::Action)
    );

    let rejection_checkpoint_request = BootstrapDispatchRequestV1 {
        request_id: "bootstrap-rejection-checkpoint".into(),
        idempotency_key: "bootstrap-rejection-checkpoint".into(),
        activation_request_id: "bootstrap-e2e-activate".into(),
        entity_type: "Temper.Example.Customer".into(),
        entity_id: "018f1f80-7b2d-7000-8000-000000000086".into(),
        initial_fields: serde_json::Map::new(),
        initial_action: Some(BootstrapActionV1 {
            action: "Reject".into(),
            parameters: serde_json::Map::new(),
        }),
    };
    store.fail_next_schema_operations(
        temper_store_sim::SimSchemaFaultPoint::RecordBootstrapActionFailure,
        1,
    );
    let checkpoint_interrupted =
        call_bootstrap(&invocation, rejection_checkpoint_request.clone()).await;
    assert!(matches!(
        checkpoint_interrupted,
        SchemaDeploymentResponseV1::Error { .. }
    ));
    let checkpoint_recovered = call_bootstrap(&invocation, rejection_checkpoint_request).await;
    let SchemaDeploymentResponseV1::Bootstrap {
        receipt: checkpoint_recovered,
    } = checkpoint_recovered
    else {
        panic!("rejection checkpoint failure should converge on retry")
    };
    assert_eq!(
        checkpoint_recovered
            .failure
            .as_ref()
            .map(|failure| failure.stage),
        Some(BootstrapFailureStageV1::Action)
    );

    let mut denied_authority = invocation.authority.clone();
    denied_authority.module_name = "worker-lookalike".into();
    denied_authority.security =
        SecurityContext::from_resolved_identity("bootstrap-unpermitted-agent", "test-agent", None);
    let denied_invocation =
        ApplicationDataInvocation::new(invocation.state.clone(), denied_authority);
    let denied = call_bootstrap(
        &denied_invocation,
        BootstrapDispatchRequestV1 {
            request_id: "bootstrap-cedar-denied".into(),
            idempotency_key: "bootstrap-cedar-denied".into(),
            activation_request_id: "bootstrap-e2e-activate".into(),
            entity_type: "Temper.Example.Customer".into(),
            entity_id: "018f1f80-7b2d-7000-8000-000000000081".into(),
            initial_fields: serde_json::Map::new(),
            initial_action: None,
        },
    )
    .await;
    let SchemaDeploymentResponseV1::Bootstrap {
        receipt: denied_receipt,
    } = denied
    else {
        panic!("Cedar denial should be retained in a bootstrap receipt: {denied:?}")
    };
    assert_eq!(denied_receipt.creation_sequence, None);
    assert_eq!(
        denied_receipt.failure.as_ref().map(|failure| failure.stage),
        Some(BootstrapFailureStageV1::Authorization)
    );
    let recovered_target = call_bootstrap(
        &invocation,
        BootstrapDispatchRequestV1 {
            request_id: "bootstrap-after-denial".into(),
            idempotency_key: "bootstrap-after-denial".into(),
            activation_request_id: "bootstrap-e2e-activate".into(),
            entity_type: "Temper.Example.Customer".into(),
            entity_id: "018f1f80-7b2d-7000-8000-000000000081".into(),
            initial_fields: serde_json::Map::new(),
            initial_action: None,
        },
    )
    .await;
    let SchemaDeploymentResponseV1::Bootstrap {
        receipt: recovered_target,
    } = recovered_target
    else {
        panic!("a pre-creation denial must release the target claim")
    };
    assert_eq!(recovered_target.creation_sequence, Some(1));

    let invalid_target_id = "018f1f80-7b2d-7000-8000-000000000082";
    let invalid = call_bootstrap(
        &invocation,
        BootstrapDispatchRequestV1 {
            request_id: "bootstrap-invalid-fields".into(),
            idempotency_key: "bootstrap-invalid-fields".into(),
            activation_request_id: "bootstrap-e2e-activate".into(),
            entity_type: "Temper.Example.Customer".into(),
            entity_id: invalid_target_id.into(),
            initial_fields: serde_json::json!({"Unknown":"value"})
                .as_object()
                .cloned()
                .unwrap(),
            initial_action: None,
        },
    )
    .await;
    let SchemaDeploymentResponseV1::Bootstrap { receipt: invalid } = invalid else {
        panic!("closure mismatch must return a durable validation receipt")
    };
    assert_eq!(
        invalid.failure.as_ref().map(|failure| failure.stage),
        Some(BootstrapFailureStageV1::Validation)
    );
    let after_invalid = call_bootstrap(
        &invocation,
        BootstrapDispatchRequestV1 {
            request_id: "bootstrap-after-invalid-fields".into(),
            idempotency_key: "bootstrap-after-invalid-fields".into(),
            activation_request_id: "bootstrap-e2e-activate".into(),
            entity_type: "Temper.Example.Customer".into(),
            entity_id: invalid_target_id.into(),
            initial_fields: serde_json::Map::new(),
            initial_action: None,
        },
    )
    .await;
    let SchemaDeploymentResponseV1::Bootstrap {
        receipt: after_invalid,
    } = after_invalid
    else {
        panic!("a pre-creation validation failure must release the target claim")
    };
    assert_eq!(after_invalid.creation_sequence, Some(1));

    let concurrent_request = BootstrapDispatchRequestV1 {
        request_id: "bootstrap-concurrent".into(),
        idempotency_key: "bootstrap-concurrent".into(),
        activation_request_id: "bootstrap-e2e-activate".into(),
        entity_type: "Temper.Example.Customer".into(),
        entity_id: "018f1f80-7b2d-7000-8000-000000000085".into(),
        initial_fields: serde_json::Map::new(),
        initial_action: Some(BootstrapActionV1 {
            action: "Rename".into(),
            parameters: serde_json::json!({"Name":"concurrent"})
                .as_object()
                .cloned()
                .unwrap(),
        }),
    };
    let (left, right) = tokio::join!(
        call_bootstrap(&invocation, concurrent_request.clone()),
        call_bootstrap(&invocation, concurrent_request),
    );
    let SchemaDeploymentResponseV1::Bootstrap { receipt: left } = left else {
        panic!("the first concurrent driver must converge")
    };
    let SchemaDeploymentResponseV1::Bootstrap { receipt: right } = right else {
        panic!("the second concurrent driver must converge")
    };
    assert_eq!(left, right);

    let mut restarted_state = ServerState::from_registry(
        ActorSystem::new("bootstrap-e2e-restart"),
        SpecRegistry::new(),
    );
    restarted_state.data_dir = tempfile::tempdir().unwrap().keep();
    restarted_state.storage_stack = Some(std::sync::Arc::new(StorageStack::from_sim(store, None)));
    let restarted = ApplicationDataInvocation::new(restarted_state, invocation.authority.clone());
    let replay = call_bootstrap(&restarted, request).await;
    let SchemaDeploymentResponseV1::Bootstrap {
        receipt: replay_receipt,
    } = replay
    else {
        panic!("cold retry should replay the dedicated receipt: {replay:?}")
    };
    assert_eq!(replay_receipt, receipt);
    let rejected_replay = call_bootstrap(&restarted, rejected_request).await;
    let SchemaDeploymentResponseV1::Bootstrap {
        receipt: rejected_replay_receipt,
    } = rejected_replay
    else {
        panic!("cold retry should replay the partial receipt: {rejected_replay:?}")
    };
    assert_eq!(rejected_replay_receipt, rejected_receipt);
}
